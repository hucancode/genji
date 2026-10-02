use anyhow::{Context, Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::agent::Agent;
use crate::config::Config;
use crate::storage::modes::Mode;
use std::path::Path;

pub mod basic {
    use anyhow::{Context, Result, anyhow, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::fmt::Write as _;
    use std::time::Duration;

    use super::{opt_bool, opt_i64, opt_str, req_str};
    use crate::agent::Agent;
    use crate::storage::proc;

    #[derive(Debug, Deserialize)]
    struct ReadArgs {
        path: String,
        offset: Option<i64>,
        limit: Option<i64>,
    }

    #[derive(Debug, Deserialize)]
    struct WriteArgs {
        path: String,
        content: String,
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ReadArgs =
            serde_json::from_value(args.clone()).context("invalid read arguments")?;
        let path = agent.resolve_path(&parsed.path);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let offset = usize::try_from(parsed.offset.unwrap_or(1).max(1))?;
        let limit = usize::try_from(parsed.limit.unwrap_or(2000).max(1))?;

        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        if total == 0 {
            return Ok(format!("{} is empty (0 lines)", path.display()));
        }
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);
        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            writeln!(out, "{:>6}\t{}", start + i + 1, line)?;
        }
        if end < total {
            writeln!(out, "\n[showing lines {}-{} of {}]", start + 1, end, total)?;
        }
        Ok(out)
    }

    pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: WriteArgs =
            serde_json::from_value(args.clone()).context("invalid write arguments")?;
        let path = agent.resolve_path(&parsed.path);
        let content = parsed.content;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        std::fs::write(&path, &content).with_context(|| format!("writing {}", path.display()))?;
        Ok(format!(
            "wrote {} bytes to {}",
            content.len(),
            agent.display_path(&path)
        ))
    }

    fn find_unique(hay: &str, needle: &str) -> Result<usize> {
        if needle.is_empty() {
            bail!("oldText must not be empty");
        }
        let mut it = hay.match_indices(needle);
        match (it.next(), it.next()) {
            (Some((i, _)), None) => Ok(i),
            (None, _) => bail!(
                "oldText not found: {:?}",
                needle.chars().take(60).collect::<String>()
            ),
            (Some(_), Some(_)) => bail!(
                "oldText is not unique: {:?}",
                needle.chars().take(60).collect::<String>()
            ),
        }
    }

    fn apply_edits(content: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
        if edits.len() == 1 && replace_all {
            let (old, new) = &edits[0];
            if old.is_empty() {
                bail!("oldText must not be empty");
            }
            let count = content.matches(old.as_str()).count();
            if count == 0 {
                bail!("oldText not found");
            }
            return Ok(content.replace(old.as_str(), new.as_str()));
        }
        let mut ranges: Vec<(usize, usize, String)> = Vec::new();
        for (old, new) in edits {
            let start = find_unique(content, old)?;
            ranges.push((start, start + old.len(), new.clone()));
        }
        let mut sorted = ranges;
        sorted.sort_by_key(|r| r.0);
        for w in sorted.windows(2) {
            if w[0].1 > w[1].0 {
                bail!("edit ranges overlap");
            }
        }
        let mut out = content.to_string();
        for (start, end, new) in sorted.into_iter().rev() {
            out.replace_range(start..end, &new);
        }
        Ok(out)
    }

    #[derive(Debug, Deserialize)]
    struct EditArgs {
        path: String,
        edits: Option<Vec<EditEntry>>,
        #[serde(rename = "oldText", alias = "old_text")] old_text: Option<String>,
        #[serde(rename = "newText", alias = "new_text")] new_text: Option<String>,
        replace_all: Option<bool>,
    }
    #[derive(Debug, Deserialize)]
    struct EditEntry {
        #[serde(rename = "oldText", alias = "old_text")] old_text: String,
        #[serde(rename = "newText", alias = "new_text")] new_text: String,
    }

    pub fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: EditArgs = super::parse_args(args)?;
        let path = agent.resolve_path(&parsed.path);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        let replace_all = parsed.replace_all.unwrap_or(false);
        let mut edits: Vec<(String, String)> = parsed.edits.unwrap_or_default()
            .into_iter().map(|e| (e.old_text, e.new_text)).collect();
        if edits.is_empty() {
            if let (Some(old), Some(new)) = (parsed.old_text, parsed.new_text) {
                edits.push((old, new));
            }
        }
        if edits.is_empty() {
            bail!("no edits supplied (provide `edits` array or `oldText`/`newText`)");
        }

        let new_content = apply_edits(&content, &edits, replace_all)?;
        std::fs::write(&path, &new_content)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(format!(
            "applied {} edit(s) to {} ({} -> {} bytes)",
            edits.len(),
            agent.display_path(&path),
            content.len(),
            new_content.len()
        ))
    }

    #[derive(Debug, Deserialize)]
    struct LsArgs { path: Option<String>, show_hidden: Option<bool>, max_depth: Option<i64> }

    pub fn ls(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: LsArgs = super::parse_args(args)?;
        let rel = parsed.path.unwrap_or_else(|| ".".into());
        let root = agent.resolve_path(&rel);
        if !root.exists() {
            bail!("path does not exist: {}", root.display());
        }
        let show_hidden = parsed.show_hidden.unwrap_or(false);
        let max_depth = parsed.max_depth.unwrap_or(0).max(0);

        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .hidden(!show_hidden)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .ignore(true)
            .parents(false)
            .require_git(false)
            .max_depth(Some(usize::try_from(max_depth + 1)?));

        let mut lines: Vec<String> = Vec::new();
        let mut dirs = 0usize;
        let mut files = 0usize;
        for entry in builder.build() {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue; // skip the root itself
            }
            let path = entry.path();
            let rel_path = path
                .strip_prefix(&agent.workspace)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs += 1;
                lines.push(format!("d        {rel_path}/"));
            } else {
                files += 1;
                lines.push(format!("f {:>8} {rel_path}", meta.len()));
            }
        }
        lines.sort();
        let mut out = lines.join("\n");
        if out.is_empty() {
            out = "(empty)".into();
        }
        out.push_str(&format!("\n\n[{dirs} dirs, {files} files under {rel}]"));
        Ok(out)
    }

    #[derive(Debug, Deserialize)]
    struct BashArgs {
        command: String,
        cwd: Option<String>,
        timeout_secs: Option<i64>,
    }

    pub fn bash(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: BashArgs =
            serde_json::from_value(args.clone()).context("invalid bash arguments")?;
        let command = parsed.command;
        let cwd = match parsed.cwd {
            Some(c) => agent.resolve_path(&c),
            None => agent.workspace.clone(),
        };
        let timeout = Duration::from_secs(
            parsed
                .timeout_secs
                .map_or(agent.cfg.bash_timeout_secs, |v| {
                    u64::try_from(v.max(1)).unwrap_or(agent.cfg.bash_timeout_secs)
                }),
        );
        let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(8192);
        let res = proc::run_bash(
            &command,
            &cwd,
            &agent.cfg.tmp_path(&agent.workspace),
            timeout,
            cap,
        )
        .with_context(|| format!("running command: {command}"))?;
        let mut out = String::new();
        out.push_str(&format!(
            "exit_code: {}\n",
            res.code.map_or_else(|| "none".into(), |c| c.to_string())
        ));
        if res.timed_out {
            out.push_str(&format!("[timed out after {}s]\n", timeout.as_secs()));
        }
        if !res.stdout.is_empty() {
            out.push_str("--- stdout ---\n");
            out.push_str(&res.stdout);
            if !res.stdout.ends_with('\n') {
                out.push('\n');
            }
        }
        if !res.stderr.is_empty() {
            out.push_str("--- stderr ---\n");
            out.push_str(&res.stderr);
            if !res.stderr.ends_with('\n') {
                out.push('\n');
            }
        }
        Ok(out)
    }

    #[cfg(test)]
    mod tests {
        use super::apply_edits;

        #[test]
        fn single_edit() {
            let out =
                apply_edits("hello world", &[("world".into(), "there".into())], false).unwrap();
            assert_eq!(out, "hello there");
        }

        #[test]
        fn multi_edit_applied_backwards() {
            let edits = vec![
                ("abc".to_string(), "x".to_string()),
                ("ghi".to_string(), "y".to_string()),
            ];
            assert_eq!(apply_edits("abcdefghi", &edits, false).unwrap(), "xdefy");
        }

        #[test]
        fn non_unique_is_error() {
            assert!(apply_edits("aa", &[("a".to_string(), "b".to_string())], false).is_err());
        }

        #[test]
        fn replace_all() {
            assert_eq!(
                apply_edits("aa", &[("a".to_string(), "b".to_string())], true).unwrap(),
                "bb"
            );
        }

        #[test]
        fn overlap_is_error() {
            let edits = vec![
                ("abc".to_string(), "x".to_string()),
                ("cde".to_string(), "y".to_string()),
            ];
            assert!(apply_edits("abcdef", &edits, false).is_err());
        }

        #[test]
        fn missing_is_error() {
            assert!(apply_edits("abc", &[("zzz".to_string(), "x".to_string())], false).is_err());
        }
    }
}
pub mod plans {
    use anyhow::{Context, Result};
    use serde::Deserialize;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    use super::parse_args;
    use crate::agent::Agent;
    use crate::config::Config;
    use crate::storage::util::slugify;

    pub fn write_plan(
        cfg: &Config,
        workspace: &Path,
        title: &str,
        content: &str,
        path: Option<&str>,
    ) -> Result<PathBuf> {
        let rel = match path {
            Some(p) => p.to_string(),
            None => format!(
                "{}/{}.md",
                cfg.plans_dir.trim_end_matches('/'),
                slugify(title, "plan")
            ),
        };
        let path = crate::storage::util::resolve_path(workspace, &rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let body = if content.trim_start().starts_with("# ") {
            content.to_string()
        } else {
            format!("# {}\n\n{}", title.trim(), content.trim_start())
        };
        std::fs::write(&path, &body).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }

    #[derive(Debug, Deserialize)]
    struct WriteArgs { title: String, content: String, path: Option<String> }

    pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: WriteArgs = parse_args(args)?;
        let title = parsed.title;
        let content = parsed.content;
        let explicit = parsed.path;
        let path = write_plan(
            &agent.cfg,
            &agent.workspace,
            &title,
            &content,
            explicit.as_deref(),
        )?;
        Ok(format!("wrote plan to {}", agent.display_path(&path)))
    }

    #[cfg(test)]
    mod tests {
        use super::write_plan;
        use crate::config::Config;
        use std::time::{SystemTime, UNIX_EPOCH};

        fn temp_workspace(tag: &str) -> std::path::PathBuf {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("genji-plans-{tag}-{nanos}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn writes_plan_under_plans_dir_with_heading() {
            let ws = temp_workspace("write");
            let cfg = Config::default();
            let path = write_plan(&cfg, &ws, "Rate limiting", "Step one.", None).unwrap();
            assert_eq!(
                path.strip_prefix(&ws).unwrap().to_string_lossy(),
                ".genji/plans/rate-limiting.md"
            );
            let text = std::fs::read_to_string(&path).unwrap();
            assert_eq!(text, "# Rate limiting\n\nStep one.");
            let _ = std::fs::remove_dir_all(&ws);
        }

        #[test]
        fn keeps_existing_heading_and_honours_explicit_path() {
            let ws = temp_workspace("path");
            let cfg = Config::default();
            let path = write_plan(
                &cfg,
                &ws,
                "Ignored",
                "# Custom\n\nBody.",
                Some("notes/my-plan.md"),
            )
            .unwrap();
            assert!(path.ends_with("notes/my-plan.md"));
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "# Custom\n\nBody.");
            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}
#[cfg(feature = "formal")]
pub mod requirements {
    use anyhow::{Result, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::Path;

    use super::{opt_i64, opt_str, req_str};
    use crate::agent::Agent;
    use crate::config::Config;
    use crate::storage::db::FieldPatch;
    use crate::storage::reqmd::{self, Requirement, RequirementLevel, RequirementStatus};

    fn fmt_req(r: &Requirement, workspace: &Path, full: bool) -> String {
        let mut s = format!("#{} [{}:{}] {}\n", r.id, r.level, r.status, r.title);
        if let Some(p) = r.parent_id {
            s.push_str(&format!("parent: #{p}\n"));
        }
        s.push_str(&format!("source: {}\n", r.source));
        s.push_str(&format!("source_path: {}\n", r.display_path(workspace)));
        let body = r.body.trim();
        if !body.is_empty() {
            s.push('\n');
            if full {
                s.push_str(body);
            } else {
                let truncated: String = body.chars().take(200).collect();
                s.push_str(&truncated);
                if body.chars().count() > 200 {
                    s.push('…');
                }
            }
            s.push('\n');
        }
        s
    }

    fn parse_level_arg(level: &str) -> Result<RequirementLevel> {
        level
            .parse()
            .map_err(|_| anyhow::anyhow!("level must be `stakeholder` or `system`"))
    }

    fn parse_status_arg(status: &str) -> Result<RequirementStatus> {
        status
            .parse()
            .map_err(|_| anyhow::anyhow!("status must be active|met|removed"))
    }

    #[derive(Debug, Deserialize)]
    struct CreateArgs { level: String, title: String, body: String, parent_id: Option<i64> }
    #[derive(Debug, Deserialize)]
    struct ReadArgs { id: Option<i64>, level: Option<String>, status: Option<String> }
    #[derive(Debug, Deserialize)]
    struct UpdateArgs { id: i64, title: Option<String>, body: Option<String>, status: Option<String>, level: Option<String>, parent_id: Option<Option<i64>> }
    #[derive(Debug, Deserialize)]
    struct RemoveArgs { id: i64, hard: Option<bool> }
    #[derive(Debug, Deserialize)]
    struct TreeArgs { status: Option<String> }
    #[derive(Debug, Deserialize)]
    struct AskArgs { question: String, requirement_id: Option<i64> }

    pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CreateArgs = super::parse_args(args)?;
        let level = parse_level_arg(&parsed.level)?;
        let title = parsed.title;
        let body = parsed.body;
        let parent_id = parsed.parent_id;
        let r = reqmd::create(
            &agent.cfg,
            &agent.workspace,
            level,
            &title,
            &body,
            parent_id,
            "agent",
        )?;
        Ok(format!(
            "created {level} requirement #{}: {} ({})",
            r.id,
            r.title,
            r.display_path(&agent.workspace)
        ))
    }

    fn read_requirements(
        cfg: &Config,
        workspace: &Path,
        id: Option<i64>,
        level: Option<RequirementLevel>,
        status: Option<RequirementStatus>,
    ) -> Result<String> {
        if let Some(id) = id {
            return match reqmd::load_by_id(cfg, workspace, id)? {
                Some(r) => Ok(fmt_req(&r, workspace, true)),
                None => bail!("requirement #{id} not found"),
            };
        }
        let reqs: Vec<Requirement> = reqmd::load_all(cfg, workspace)?
            .into_iter()
            .filter(|r| level.is_none_or(|l| r.level == l))
            .filter(|r| status.is_none_or(|s| r.status == s))
            .collect();
        if reqs.is_empty() {
            return Ok("(no requirements)".into());
        }
        let full = reqs.len() == 1;
        let mut out = String::new();
        for r in &reqs {
            out.push_str(&fmt_req(r, workspace, full));
            out.push_str("---\n");
        }
        Ok(out)
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ReadArgs = super::parse_args(args)?;
        let level = parsed.level.as_deref().map(parse_level_arg).transpose()?;
        let status = parsed.status.as_deref().map(parse_status_arg).transpose()?;
        read_requirements(&agent.cfg, &agent.workspace, parsed.id, level, status)
    }

    pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: UpdateArgs = super::parse_args(args)?;
        let id = parsed.id;
        let status = parsed.status.as_deref().map(parse_status_arg).transpose()?;
        let level = parsed.level.as_deref().map(parse_level_arg).transpose()?;
        let parent_id = match parsed.parent_id {
            None => FieldPatch::Keep,
            Some(None) => FieldPatch::Clear,
            Some(Some(v)) => FieldPatch::Set(v),
        };
        if !reqmd::update(
            &agent.cfg,
            &agent.workspace,
            id,
            parsed.title.as_deref(),
            parsed.body.as_deref(),
            status,
            level,
            parent_id,
        )? {
            bail!("requirement #{id} not found");
        }
        Ok(format!("updated requirement #{id}"))
    }

    pub fn remove(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: RemoveArgs = super::parse_args(args)?;
        let id = parsed.id;
        let hard = parsed.hard.unwrap_or(false);
        if !reqmd::remove(&agent.cfg, &agent.workspace, id, hard)? {
            bail!("requirement #{id} not found");
        }
        Ok(format!(
            "{} requirement #{id}",
            if hard { "deleted" } else { "removed" }
        ))
    }

    pub fn tree(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: TreeArgs = super::parse_args(args)?;
        let filter = parsed.status.as_deref()
            .map(parse_status_arg).transpose()?;
        let reqs: Vec<Requirement> = reqmd::load_all(&agent.cfg, &agent.workspace)?
            .into_iter()
            .filter(|r| filter.is_none_or(|s| r.status == s))
            .collect();
        if reqs.is_empty() {
            return Ok("(no requirements)".into());
        }

        let tickets = super::tickets::list_all(agent)?;
        let mut open: BTreeMap<i64, i64> = BTreeMap::new();
        let mut done: BTreeMap<i64, i64> = BTreeMap::new();
        for t in &tickets {
            if let Some(rid) = t.requirement_id {
                if t.status.is_open() {
                    *open.entry(rid).or_default() += 1;
                } else if t.status.is_done() {
                    *done.entry(rid).or_default() += 1;
                }
            }
        }

        let ids: BTreeSet<i64> = reqs.iter().map(|r| r.id).collect();
        let mut children: BTreeMap<Option<i64>, Vec<&Requirement>> = BTreeMap::new();
        for r in &reqs {
            let parent = match r.parent_id {
                Some(p) if ids.contains(&p) => Some(p),
                _ => None,
            };
            children.entry(parent).or_default().push(r);
        }

        let mut out = String::new();
        out.push_str(&format!(
            "requirements: {} active / {} total, tickets: {} open / {} done\n\n",
            reqs.iter()
                .filter(|r| r.status == RequirementStatus::Active)
                .count(),
            reqs.len(),
            tickets.iter().filter(|t| t.status.is_open()).count(),
            tickets.iter().filter(|t| t.status.is_done()).count(),
        ));
        let mut visited = BTreeSet::new();
        if let Some(roots) = children.get(&None) {
            for r in roots {
                render_tree(r, &children, &open, &done, 0, &mut visited, &mut out);
            }
        }
        // Safety net for cycles: show anything not reachable from a root.
        for r in &reqs {
            if !visited.contains(&r.id) {
                render_tree(r, &children, &open, &done, 0, &mut visited, &mut out);
            }
        }
        Ok(out)
    }

    fn render_tree<'a>(
        req: &'a Requirement,
        children: &BTreeMap<Option<i64>, Vec<&'a Requirement>>,
        open: &BTreeMap<i64, i64>,
        done: &BTreeMap<i64, i64>,
        depth: usize,
        visited: &mut BTreeSet<i64>,
        out: &mut String,
    ) {
        if !visited.insert(req.id) {
            return;
        }
        let indent = "  ".repeat(depth);
        let o = open.get(&req.id).copied().unwrap_or(0);
        let d = done.get(&req.id).copied().unwrap_or(0);
        out.push_str(&format!(
            "{indent}#{} [{}:{}] {}  (tickets: {o} open / {d} done)\n",
            req.id, req.level, req.status, req.title
        ));
        if let Some(kids) = children.get(&Some(req.id)) {
            for k in kids {
                render_tree(k, children, open, done, depth + 1, visited, out);
            }
        }
    }

    pub fn ask(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: AskArgs = super::parse_args(args)?;
        let question = parsed.question;
        let requirement_id = parsed.requirement_id;
        let qid = agent
            .db
            .question_ask(requirement_id, &agent.instance_id, &question)?;

        Ok(format!(
            "recorded question #{qid}: {question}\n(no interactive user available; relay this question to the user)"
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::read_requirements;
        use crate::config::Config;
        use crate::storage::db::FieldPatch;
        use crate::storage::reqmd::{self, RequirementLevel, RequirementStatus};
        use std::time::{SystemTime, UNIX_EPOCH};

        fn temp_workspace(tag: &str) -> std::path::PathBuf {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let dir = std::env::temp_dir().join(format!("genji-reqtools-{tag}-{nanos}"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        #[test]
        fn reads_one_by_id_and_lists_with_filters() {
            let ws = temp_workspace("read");
            let cfg = Config::default();
            let parent = reqmd::create(
                &cfg,
                &ws,
                RequirementLevel::Stakeholder,
                "Cat Classifier",
                "Must classify cats.",
                None,
                "agent",
            )
            .unwrap();
            let child = reqmd::create(
                &cfg,
                &ws,
                RequirementLevel::System,
                "Accept URLs",
                "Accept image URLs.",
                Some(parent.id),
                "agent",
            )
            .unwrap();
            reqmd::update(
                &cfg,
                &ws,
                child.id,
                None,
                None,
                Some(RequirementStatus::Met),
                None,
                FieldPatch::Keep,
            )
            .unwrap();

            // By id: the full requirement body, with no filtering applied.
            let one = read_requirements(&cfg, &ws, Some(parent.id), None, None).unwrap();
            assert!(
                one.contains("#1 [stakeholder:active] Cat Classifier"),
                "{one}"
            );
            assert!(one.contains("Must classify cats."), "{one}");

            // No id: list everything.
            let all = read_requirements(&cfg, &ws, None, None, None).unwrap();
            assert!(
                all.contains("#1 [stakeholder:active] Cat Classifier"),
                "{all}"
            );
            assert!(all.contains("#2 [system:met] Accept URLs"), "{all}");

            // Filter by level.
            let sys =
                read_requirements(&cfg, &ws, None, Some(RequirementLevel::System), None).unwrap();
            assert!(sys.contains("#2 "), "{sys}");
            assert!(!sys.contains("#1 "), "{sys}");

            // Filter by status.
            let met =
                read_requirements(&cfg, &ws, None, None, Some(RequirementStatus::Met)).unwrap();
            assert!(met.contains("#2 "), "{met}");
            assert!(!met.contains("#1 "), "{met}");

            // A filter with no matches is an empty list, not an error.
            let none =
                read_requirements(&cfg, &ws, None, None, Some(RequirementStatus::Removed)).unwrap();
            assert_eq!(none, "(no requirements)");

            // A missing id is an error, not an empty list.
            assert!(read_requirements(&cfg, &ws, Some(999), None, None).is_err());

            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}
pub mod retro {
    use anyhow::{Result, bail};
    use rusqlite::params;
    use serde_json::Value;

    use super::{opt_bool, opt_i64, opt_str, req_str};
    use crate::agent::Agent;
    use crate::storage::db::PromptVersionRow;
    use crate::storage::modes::Mode;

    pub fn instances(agent: &mut Agent, args: &Value) -> Result<String> {
        let limit = opt_i64(args, "limit").unwrap_or(20).clamp(1, 500);
        let mode = opt_str(args, "mode");
        let mut stmt = agent.db.conn.prepare(
            "SELECT id,mode,parent_instance,depth,status,tokens_used,started_at,substr(COALESCE(task,''),1,80) FROM instances WHERE (?1 IS NULL OR mode = ?1) ORDER BY started_at DESC LIMIT ?2",
        )?;
        let rows = stmt.query_map(params![mode, limit], |r| {
            Ok(format!(
                "{}\t{}\tdepth={}\t{}\ttokens={}\t{}\t{}\n",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            out.push_str(&row?);
        }
        if out.is_empty() {
            out = "(no instances)".into();
        }
        Ok(out)
    }

    pub fn instance(agent: &mut Agent, args: &Value) -> Result<String> {
        let sid = req_str(args, "instance_id")?;
        let limit = opt_i64(args, "limit").unwrap_or(200).clamp(1, 2000);
        let mut stmt = agent.db.conn.prepare(
        "SELECT seq,role,content,tool_calls FROM messages WHERE instance_id=? ORDER BY seq LIMIT ?",
    )?;
        let rows = stmt.query_map(params![sid, limit], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            let (seq, role, content, tc) = row?;
            let snippet: String = content.chars().take(500).collect();
            out.push_str(&format!("#{seq} [{role}] {snippet}\n"));
            if let Some(tc) = tc
                && tc != "null"
                && !tc.is_empty()
            {
                out.push_str(&format!(
                    "    tool_calls: {}\n",
                    tc.chars().take(200).collect::<String>()
                ));
            }
        }
        if out.is_empty() {
            out = format!("(instance {sid} has no messages)");
        }
        Ok(out)
    }

    pub fn messages(agent: &mut Agent, args: &Value) -> Result<String> {
        let limit = opt_i64(args, "limit").unwrap_or(50).clamp(1, 500);
        let instance_id = opt_str(args, "instance_id");
        let role = opt_str(args, "role");
        let search = opt_str(args, "search").map(|q| format!("%{q}%"));
        let mut stmt = agent.db.conn.prepare(
            "SELECT instance_id,seq,role,substr(content,1,400),created_at FROM messages WHERE (?1 IS NULL OR instance_id = ?1) AND (?2 IS NULL OR role = ?2) AND (?3 IS NULL OR content LIKE ?3) ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![instance_id, role, search, limit], |r| {
            Ok(format!(
                "{}\t#{} [{}]\t{}\t{}\n",
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(3)?
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            out.push_str(&row?);
        }
        if out.is_empty() {
            out = "(no matching messages)".into();
        }
        Ok(out)
    }

    pub fn tool_calls(agent: &mut Agent, args: &Value) -> Result<String> {
        let limit = opt_i64(args, "limit").unwrap_or(50).clamp(1, 500);
        let instance_id = opt_str(args, "instance_id");
        let name = opt_str(args, "name");
        let errors_only = opt_bool(args, "errors_only").unwrap_or(false);
        let mut stmt = agent.db.conn.prepare(
            "SELECT instance_id,name,is_error,duration_ms,substr(args,1,160),substr(result,1,300),created_at FROM tool_calls WHERE (?1 IS NULL OR instance_id = ?1) AND (?2 IS NULL OR name = ?2) AND (?3 = 0 OR is_error = 1) ORDER BY id DESC LIMIT ?4",
        )?;
        let rows = stmt.query_map(params![instance_id, name, errors_only, limit], |r| {
            Ok(format!(
                "{}\t{}\terr={}\t{}ms\t{}\n  args: {}\n  result: {}\n",
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            out.push_str(&row?);
        }
        if out.is_empty() {
            out = "(no matching tool calls)".into();
        }
        Ok(out)
    }

    pub fn stats(agent: &mut Agent, _args: &Value) -> Result<String> {
        let mut out = String::new();
        let (instances, tokens): (i64, i64) = agent.db.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_used),0) FROM instances",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        out.push_str(&format!("instances: {instances}, total tokens: {tokens}\n"));
        out.push_str("instances by mode:\n");
        {
            let mut stmt = agent
                .db
                .conn
                .prepare("SELECT mode,COUNT(*) FROM instances GROUP BY mode ORDER BY 2 DESC")?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (m, c) = row?;
                out.push_str(&format!("  {m}: {c}\n"));
            }
        }
        out.push_str("tool calls (name, count, errors, avg_ms):\n");
        {
            let mut stmt = agent.db.conn.prepare(
                "SELECT name,COUNT(*),COALESCE(SUM(is_error),0),CAST(AVG(duration_ms) AS INT)
             FROM tool_calls GROUP BY name ORDER BY 2 DESC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?;
            for row in rows {
                let (n, c, e, d) = row?;
                out.push_str(&format!("  {n}: calls={c} errors={e} avg={d}ms\n"));
            }
        }
        out.push_str("skills by loads:\n");
        {
            let mut stmt = agent.db.conn.prepare(
            "SELECT skill_name,COUNT(*) FROM skill_loads GROUP BY skill_name ORDER BY 2 DESC LIMIT 20",
        )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            let mut any = false;
            for row in rows {
                let (n, c) = row?;
                any = true;
                out.push_str(&format!("  {n}: {c}\n"));
            }
            if !any {
                out.push_str("  (none)\n");
            }
        }
        let compactions: i64 =
            agent
                .db
                .conn
                .query_row("SELECT COUNT(*) FROM compactions", [], |r| r.get(0))?;
        out.push_str(&format!("compactions: {compactions}\n"));
        Ok(out)
    }

    fn parse_mode(s: &str) -> Result<Mode> {
        let mode = Mode::parse(s)?;
        if !mode.allows_extended() {
            bail!(
                "mode `{}` does not support an extended prompt",
                mode.as_str()
            );
        }
        Ok(mode)
    }

    pub fn prompt_read(agent: &mut Agent, args: &Value) -> Result<String> {
        let mode = parse_mode(&req_str(args, "mode")?)?;
        match agent.db.prompt_active(mode.as_str())? {
            Some(p) => Ok(format!(
                "mode={} version={} author={} created={}\n\n{}",
                mode.as_str(),
                p.version,
                p.author,
                p.created_at,
                p.content
            )),
            None => Ok(format!("(no active extended prompt for {})", mode.as_str())),
        }
    }

    pub fn prompt_edit(agent: &mut Agent, args: &Value) -> Result<String> {
        let mode = parse_mode(&req_str(args, "mode")?)?;
        let content = req_str(args, "content")?;
        let reason = opt_str(args, "reason").unwrap_or_default();
        let version = agent
            .db
            .prompt_add_version(mode.as_str(), &content, "retro", &reason)?;
        // Keep the in-memory system prompt fresh if we edited our own mode.
        agent.refresh_system_prompt()?;
        Ok(format!(
            "updated extended prompt for `{}` to v{version}",
            mode.as_str()
        ))
    }

    pub fn prompt_history(agent: &mut Agent, args: &Value) -> Result<String> {
        let mode = parse_mode(&req_str(args, "mode")?)?;
        let versions: Vec<PromptVersionRow> = agent.db.prompt_versions(mode.as_str())?;
        if versions.is_empty() {
            return Ok(format!("(no versions for {})", mode.as_str()));
        }
        let mut out = String::new();
        for v in versions {
            out.push_str(&format!(
                "v{}\t{}\tactive={}\tby {}\t{}\t{}B\n",
                v.version,
                v.created_at,
                v.active,
                v.author,
                v.reason,
                v.content.len()
            ));
        }
        Ok(out)
    }

    pub fn prompt_rollback(agent: &mut Agent, args: &Value) -> Result<String> {
        let mode = parse_mode(&req_str(args, "mode")?)?;
        let version = opt_i64(args, "version").ok_or_else(|| anyhow::anyhow!("missing version"))?;
        if !agent.db.prompt_activate(mode.as_str(), version)? {
            bail!("mode {} has no version {}", mode.as_str(), version);
        }
        agent.refresh_system_prompt()?;
        Ok(format!(
            "activated v{version} for `{}` extended prompt",
            mode.as_str()
        ))
    }
}
pub mod skills {
    use anyhow::{Context, Result, bail};
    use serde_json::Value;
    use std::path::Path;

    use super::{opt_i64, opt_str, req_str};
    use crate::agent::Agent;
    use crate::storage::db::Db;

    pub fn parse_skill(text: &str, fallback_name: &str) -> (String, String, String) {
        let mut name = fallback_name.to_string();
        let mut description = String::new();
        let body;
        if let Some(rest) = text.strip_prefix("---\n")
            && let Some(idx) = rest.find("\n---")
        {
            let front = &rest[..idx];
            for line in front.lines() {
                if let Some(v) = line.strip_prefix("name:") {
                    name = v.trim().trim_matches('"').to_string();
                } else if let Some(v) = line.strip_prefix("description:") {
                    description = v.trim().trim_matches('"').to_string();
                }
            }
            body = rest[idx + 4..].trim_start_matches('\n').to_string();
            return (name, description, body);
        }
        body = text.to_string();
        (name, description, body)
    }

    fn render_skill(name: &str, description: &str, content: &str) -> String {
        format!(
            "---\nname: {name}\ndescription: {}\n---\n\n{}\n",
            description.replace('\n', " "),
            content.trim_end()
        )
    }

    pub fn sync_skills(db: &Db, dir: &Path) -> Result<usize> {
        if !dir.exists() {
            return Ok(0);
        }
        let mut count = 0;
        for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("md") {
                continue;
            }
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_string();
            let text = std::fs::read_to_string(&path)?;
            let (name, desc, body) = parse_skill(&text, &stem);
            let existing = db.skill_get(&name)?;
            let changed = match &existing {
                Some(e) => e.content != body || e.description != desc,
                None => true,
            };
            db.skill_upsert(&name, &path.to_string_lossy(), &desc, &body)?;
            if changed {
                db.skill_version_add(&name, &body, &desc, "file", "synced from file")?;
            }
            count += 1;
        }
        Ok(count)
    }

    fn write_skill_file(agent: &Agent, name: &str, description: &str, content: &str) -> Result<()> {
        let dir = agent.cfg.skills_path(&agent.workspace);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(format!("{name}.md"));
        std::fs::write(&path, render_skill(name, description, content))
            .with_context(|| format!("writing {}", path.display()))?;
        Ok(())
    }

    fn save_skill(
        agent: &Agent,
        name: &str,
        description: &str,
        content: &str,
        author: &str,
        reason: &str,
    ) -> Result<()> {
        let dir = agent.cfg.skills_path(&agent.workspace);
        let path = dir.join(format!("{name}.md"));
        agent
            .db
            .skill_upsert(name, &path.to_string_lossy(), description, content)?;
        agent
            .db
            .skill_version_add(name, content, description, author, reason)?;
        write_skill_file(agent, name, description, content)?;
        Ok(())
    }

    pub fn load(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        let skill = agent.db.skill_get(&name)?;
        if let Some(s) = skill {
            agent.db.skill_record_load(&agent.instance_id, &name)?;
            Ok(format!(
                "# Skill: {}\n{}\n\n{}",
                s.name, s.description, s.content
            ))
        } else {
            let names: Vec<String> = agent.db.skill_list()?.into_iter().map(|s| s.name).collect();
            bail!(
                "skill `{name}` not found. available: {}",
                if names.is_empty() {
                    "(none)".into()
                } else {
                    names.join(", ")
                }
            )
        }
    }

    pub fn list(agent: &mut Agent, _args: &Value) -> Result<String> {
        let skills = agent.db.skill_list()?;
        if skills.is_empty() {
            return Ok("(no skills)".into());
        }
        let mut out = String::new();
        for s in skills {
            out.push_str(&format!(
                "- {} (uses={}): {}\n",
                s.name,
                s.uses,
                if s.description.is_empty() {
                    "(no description)"
                } else {
                    &s.description
                }
            ));
        }
        Ok(out)
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        if let Some(v) = opt_i64(args, "version") {
            return match agent.db.skill_version_get(&name, v)? {
                Some((content, desc)) => Ok(format!("# {name} v{v}\n{desc}\n\n{content}")),
                None => bail!("skill `{name}` has no version {v}"),
            };
        }
        let s = agent
            .db
            .skill_get(&name)?
            .ok_or_else(|| anyhow::anyhow!("skill `{name}` not found"))?;
        Ok(format!(
            "# {}\npath: {}\nuses: {}\ndescription: {}\n\n{}",
            s.name, s.path, s.uses, s.description, s.content
        ))
    }

    pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        if name.contains('/') || name.contains("..") {
            bail!("invalid skill name");
        }
        let description = opt_str(args, "description").unwrap_or_default();
        let content = req_str(args, "content")?;
        let reason = opt_str(args, "reason").unwrap_or_default();
        let existed = agent.db.skill_get(&name)?.is_some();
        save_skill(agent, &name, &description, &content, "retro", &reason)?;
        Ok(format!(
            "{} skill `{name}` ({} bytes)",
            if existed { "updated" } else { "created" },
            content.len()
        ))
    }

    pub fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        let old = req_str(args, "oldText")?;
        let new = req_str(args, "newText")?;
        let reason = opt_str(args, "reason").unwrap_or_default();
        let s = agent
            .db
            .skill_get(&name)?
            .ok_or_else(|| anyhow::anyhow!("skill `{name}` not found"))?;
        let count = s.content.matches(&old).count();
        if count == 0 {
            bail!("oldText not found in skill `{name}`");
        }
        if count > 1 {
            bail!("oldText is not unique in skill `{name}` ({count} matches)");
        }
        let updated = s.content.replacen(&old, &new, 1);
        save_skill(agent, &name, &s.description, &updated, "retro", &reason)?;
        Ok(format!("edited skill `{name}`"))
    }

    pub fn history(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        let mut stmt = agent.db.conn.prepare(
            "SELECT version,author,reason,length(content),created_at FROM skill_versions
         WHERE skill_name=? ORDER BY version DESC",
        )?;
        let rows = stmt.query_map(rusqlite::params![name], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?;
        let mut out = String::new();
        for row in rows {
            let (v, author, reason, len, at) = row?;
            out.push_str(&format!("v{v}\t{at}\tby {author}\t{len}B\t{reason}\n"));
        }
        if out.is_empty() {
            out = format!("(no versions for `{name}`)");
        }
        Ok(out)
    }

    pub fn rollback(agent: &mut Agent, args: &Value) -> Result<String> {
        let name = req_str(args, "name")?;
        let version = opt_i64(args, "version").ok_or_else(|| anyhow::anyhow!("missing version"))?;
        let (content, desc) = agent
            .db
            .skill_version_get(&name, version)?
            .ok_or_else(|| anyhow::anyhow!("skill `{name}` has no version {version}"))?;
        save_skill(
            agent,
            &name,
            &desc,
            &content,
            "rollback",
            &format!("rolled back to v{version}"),
        )?;
        Ok(format!("rolled skill `{name}` back to v{version}"))
    }

    #[cfg(test)]
    mod tests {
        use super::parse_skill;

        #[test]
        fn parses_frontmatter() {
            let text = "---\nname: foo\ndescription: bar baz\n---\n\n# Body\ncontent\n";
            let (n, d, b) = parse_skill(text, "fallback");
            assert_eq!(n, "foo");
            assert_eq!(d, "bar baz");
            assert!(b.contains("Body"));
        }

        #[test]
        fn falls_back_without_frontmatter() {
            let (n, _d, b) = parse_skill("just content", "stem");
            assert_eq!(n, "stem");
            assert_eq!(b, "just content");
        }
    }
}
pub mod spawn {
    use anyhow::{Result, bail};
    use serde_json::{Value, json};
    use std::path::{Path, PathBuf};
    use std::time::Duration;

    use super::{opt_str, req_str};
    use crate::agent::Agent;
    use crate::storage::proc;

    pub fn spawn(agent: &mut Agent, args: &Value) -> Result<String> {
        let mode = req_str(args, "mode")?;
        if !["plan", "build", "explore"].contains(&mode.as_str()) {
            bail!("spawn mode must be plan|build|explore (not retro)");
        }
        if agent.depth >= agent.cfg.max_subagent_depth {
            bail!(
                "subagent depth limit reached ({} >= {})",
                agent.depth,
                agent.cfg.max_subagent_depth
            );
        }
        let instructions = req_str(args, "instructions")?;
        let task = opt_str(args, "task").unwrap_or_else(|| format!("subagent:{mode}"));

        let exe = std::env::current_exe().unwrap_or_else(|_| "genji".into());
        let instruction_file = InstructionFile::create(&agent.workspace, &instructions)?;
        let cmd_args = build_subagent_args(
            &mode,
            &agent.instance_id,
            instruction_file.path(),
            &task,
            agent.depth,
            agent.formal,
        );

        let cap = agent.cfg.tool_result_max_bytes.saturating_mul(2).max(16384);
        let res = proc::run_capture(
            &exe.to_string_lossy(),
            &cmd_args,
            &agent.workspace,
            &agent.cfg.tmp_path(&agent.workspace),
            Duration::from_secs(agent.cfg.spawn_timeout_secs),
            cap,
        )?;

        let ParsedEvents {
            parsed,
            sub_instance,
            saw_end,
            start,
            mut end,
            mut errors,
            tools,
        } = parse_subagent_events(&res.stdout);

        if parsed == 0 {
            let text = if res.stdout.trim().is_empty() {
                res.stderr.trim().to_string()
            } else {
                res.stdout.trim().to_string()
            };
            end = Some(json!({
                "type": "instance_end",
                "status": if res.timed_out { "timed_out" } else { "unknown" },
                "report": text,
            }));
        } else if !saw_end {
            errors.push(json!({
                "type": "error",
                "message": "subagent event stream ended without instance_end",
            }));
        }
        if res.timed_out {
            if let Some(e) = end.as_mut() {
                e["status"] = json!("timed_out");
            }
            errors.push(json!({ "type": "error", "message": "subagent timed out" }));
        }

        let events = bounded_events(
            start,
            tools,
            errors,
            end,
            agent
                .cfg
                .tool_result_max_bytes
                .saturating_sub(1024)
                .max(4096),
        );
        let status = events
            .iter()
            .rev()
            .find(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
            .and_then(|e| e.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("incomplete")
            .to_string();

        Ok(json!({
            "subagent_instance": if sub_instance.is_empty() {
                Value::Null
            } else {
                json!(sub_instance)
            },
            "mode": mode,
            "status": status,
            "exit_code": res.code,
            "timed_out": res.timed_out,
            "duration_ms": res.duration_ms as u64,
            "events": events,
        })
        .to_string())
    }

    struct InstructionFile {
        path: PathBuf,
    }

    impl InstructionFile {
        fn create(workspace: &Path, instructions: &str) -> Result<Self> {
            let dir = workspace.join(".genji").join("tmp");
            std::fs::create_dir_all(&dir)?;
            let path = dir.join(format!(
                "subagent-{}-{}.md",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            ));
            std::fs::write(&path, instructions)?;
            Ok(Self { path })
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for InstructionFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn build_subagent_args(
        mode: &str,
        parent_instance: &str,
        instructions: &Path,
        task: &str,
        depth: u32,
        formal: bool,
    ) -> Vec<String> {
        let mut args = vec![
            mode.to_string(),
            "--subagent".to_string(),
            "--parent-instance".to_string(),
            parent_instance.to_string(),
            "--instructions-file".to_string(),
            instructions.to_string_lossy().into_owned(),
            "--label".to_string(),
            task.to_string(),
            "--depth".to_string(),
            (depth + 1).to_string(),
            "--quiet-startup".to_string(),
            "--no-control".to_string(),
        ];
        // Subagents inherit Formal so a build subagent can work the same tickets.
        if formal {
            args.push("--formal".to_string());
        }
        args
    }

    struct ParsedEvents {
        parsed: usize,
        sub_instance: String,
        saw_end: bool,
        start: Option<Value>,
        end: Option<Value>,
        errors: Vec<Value>,
        tools: Vec<Value>,
    }

    fn parse_subagent_events(stdout: &str) -> ParsedEvents {
        let mut result = ParsedEvents {
            parsed: 0,
            sub_instance: String::new(),
            saw_end: false,
            start: None,
            end: None,
            errors: Vec::new(),
            tools: Vec::new(),
        };
        for line in stdout.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(event) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            if event.get("type").is_none() {
                continue;
            }
            result.parsed += 1;
            if result.sub_instance.is_empty()
                && let Some(s) = event.get("instance").and_then(|s| s.as_str())
            {
                result.sub_instance = s.to_string();
            }
            match event.get("type").and_then(|t| t.as_str()) {
                Some("instance_start") => result.start = Some(event),
                Some("instance_end") => {
                    result.saw_end = true;
                    result.end = Some(event);
                }
                Some("error") => result.errors.push(event),
                Some("tool_call" | "tool_result") => result.tools.push(event),
                _ => {}
            }
        }
        result
    }

    fn bounded_events(
        start: Option<Value>,
        tools: Vec<Value>,
        errors: Vec<Value>,
        end: Option<Value>,
        budget: usize,
    ) -> Vec<Value> {
        let end = end.unwrap_or_else(|| json!({ "type": "instance_end", "status": "incomplete" }));
        let mut events: Vec<Value> = Vec::new();
        if let Some(s) = start {
            events.push(s);
        }
        events.extend(tools);
        events.extend(errors);
        events.push(end);

        // Clip bulky string fields.
        const CLIP: usize = 2000;
        for ev in &mut events {
            for field in ["result", "content", "reasoning", "summary", "message"] {
                if let Some(s) = ev.get(field).and_then(|v| v.as_str())
                    && s.len() > CLIP
                {
                    ev[field] = json!(crate::llm::truncate(s, CLIP));
                }
            }
        }

        let size = |evs: &[Value]| serde_json::to_string(evs).map_or(usize::MAX, |s| s.len());
        // Drop tool detail (oldest first) while keeping start/errors/end.
        while size(&events) > budget {
            let idx = events.iter().position(|e| {
                matches!(
                    e.get("type").and_then(|t| t.as_str()),
                    Some("tool_call" | "tool_result")
                )
            });
            match idx {
                Some(i) => {
                    events.remove(i);
                }
                None => break,
            }
        }
        // Last resort: shrink the report so `instance_end` still fits.
        while size(&events) > budget {
            let Some(i) = events
                .iter()
                .rposition(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
            else {
                break;
            };
            let report_len = events[i]
                .get("report")
                .and_then(|v| v.as_str())
                .map_or(0, str::len);
            if report_len <= 64 {
                break;
            }
            let current = events[i]
                .get("report")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let shrunk = crate::llm::truncate(&current, report_len / 2);
            events[i]["report"] = json!(shrunk);
        }
        // Absolute last resort: keep only `instance_end`.
        if size(&events) > budget
            && let Some(i) = events
                .iter()
                .rposition(|e| e.get("type").and_then(|t| t.as_str()) == Some("instance_end"))
        {
            let end = events.remove(i);
            events.clear();
            events.push(end);
        }
        events
    }
}
#[cfg(feature = "formal")]
pub mod tickets {
    use anyhow::{Result, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::path::Path;

    use super::{opt_i64, opt_str, req_str};
    use crate::agent::Agent;
    use crate::storage::db::{FieldPatch, TicketEdit};
    use crate::storage::ticketmd::{self, Ticket, TicketStatus};

    fn find(agent: &Agent, id: i64) -> Result<Option<Ticket>> {
        ticketmd::load_by_id(&agent.cfg, &agent.workspace, id)
    }

    pub fn list_all(agent: &Agent) -> Result<Vec<Ticket>> {
        ticketmd::load_all(&agent.cfg, &agent.workspace)
    }

    fn fmt_ticket(t: &Ticket, workspace: &Path) -> String {
        let mut s = format!("#{} [{}] {}\n", t.id, t.status, t.title);
        if t.priority != 2 {
            s.push_str(&format!("priority: {}\n", t.priority));
        }
        if let Some(r) = t.requirement_id {
            s.push_str(&format!("requirement: #{r}\n"));
        }
        if let Some(p) = t.parent_id {
            s.push_str(&format!("parent: #{p}\n"));
        }
        s.push_str(&format!("path: {}\n", t.display_path(workspace)));
        s.push_str(&format!("created: {}\n", t.created_at));
        if let Some(r) = &t.resolution {
            s.push_str(&format!("resolution: {r}\n"));
        }
        if !t.description.trim().is_empty() {
            s.push('\n');
            s.push_str(t.description.trim());
            s.push('\n');
        }
        s
    }

    #[derive(Debug, Deserialize)]
    struct CreateArgs { title: String, description: Option<String>, priority: Option<i64>, parent_id: Option<i64>, requirement_id: Option<i64> }
    #[derive(Debug, Deserialize)]
    struct ReadArgs { id: Option<i64>, status: Option<String>, requirement_id: Option<i64> }
    #[derive(Debug, Deserialize)]
    struct ClaimArgs { id: Option<i64>, requirement_id: Option<i64> }
    #[derive(Debug, Deserialize)]
    struct UpdateArgs { id: i64, title: Option<String>, description: Option<String>, priority: Option<i64>, parent_id: Option<Option<i64>>, requirement_id: Option<Option<i64>>, status: Option<String>, resolution: Option<String> }
    #[derive(Debug, Deserialize)]
    struct CloseArgs { id: i64, reason: Option<String> }

    pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CreateArgs = super::parse_args(args)?;
        let title = parsed.title;
        let description = parsed.description.unwrap_or_default();
        let priority = parsed.priority.unwrap_or(2).clamp(1, 3);
        let parent_id = parsed.parent_id;
        let requirement_id = parsed.requirement_id;
        let mode = agent.mode.as_str().to_string();
        let t = ticketmd::create(
            &agent.cfg,
            &agent.workspace,
            &title,
            &description,
            priority,
            parent_id,
            requirement_id,
            &mode,
        )?;
        Ok(format!("created ticket #{}: {}", t.id, t.title))
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ReadArgs = super::parse_args(args)?;
        if let Some(id) = parsed.id {
            return match find(agent, id)? {
                Some(t) => Ok(fmt_ticket(&t, &agent.workspace)),
                None => bail!("ticket #{id} not found"),
            };
        }
        let status = parsed.status.as_deref()
            .map(parse_ticket_status).transpose()?;
        let requirement_id = parsed.requirement_id;
        let tickets: Vec<Ticket> = ticketmd::load_all(&agent.cfg, &agent.workspace)?
            .into_iter()
            .filter(|t| status.is_none_or(|s| t.status == s))
            .filter(|t| requirement_id.is_none_or(|r| t.requirement_id == Some(r)))
            .collect();
        if tickets.is_empty() {
            return Ok("(no tickets)".into());
        }
        let mut out = String::new();
        for t in &tickets {
            out.push_str(&fmt_ticket(t, &agent.workspace));
            out.push_str("---\n");
        }
        Ok(out)
    }

    pub fn claim(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ClaimArgs = super::parse_args(args)?;
        let id = parsed.id;
        let requirement_id = parsed.requirement_id;
        let ticket = match id {
            Some(id) => find(agent, id)?,
            None => list_all(agent)?.into_iter().find(|t| {
                t.status == TicketStatus::Open
                    && requirement_id.is_none_or(|r| t.requirement_id == Some(r))
            }),
        };
        let Some(t) = ticket else {
            return Ok(match id {
                Some(id) => format!("(no claimable ticket #{id})"),
                None => "(no open tickets to claim)".to_string(),
            });
        };
        if !t.status.is_open() {
            bail!("ticket #{} is {} and cannot be claimed", t.id, t.status);
        }
        let Some(t) = ticketmd::update(
            &agent.cfg,
            &agent.workspace,
            t.id,
            &TicketEdit::default(),
            Some(TicketStatus::InProgress),
            None,
        )?
        else {
            bail!("ticket #{} is archived and cannot be claimed", t.id);
        };
        Ok(format!(
            "claimed ticket #{}\n\n{}",
            t.id,
            fmt_ticket(&t, &agent.workspace)
        ))
    }

    fn parse_ticket_status(status: &str) -> Result<TicketStatus> {
        status
            .parse()
            .map_err(|_| anyhow::anyhow!("status must be open|in_progress|resolved|closed"))
    }

    pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: UpdateArgs = super::parse_args(args)?;
        let id = parsed.id;
        let status = parsed.status.as_deref()
            .map(parse_ticket_status).transpose()?;
        let edit = TicketEdit {
            title: parsed.title,
            description: parsed.description,
            priority: parsed.priority.map(|p| p.clamp(1, 3)),
            parent_id: args
                .get("parent_id")
                .map(|v| v.as_i64().map_or(FieldPatch::Clear, FieldPatch::Set))
                .unwrap_or_default(),
            requirement_id: args
                .get("requirement_id")
                .map(|v| v.as_i64().map_or(FieldPatch::Clear, FieldPatch::Set))
                .unwrap_or_default(),
        };
        let resolution = parsed.resolution;
        ticketmd::update(
            &agent.cfg,
            &agent.workspace,
            id,
            &edit,
            status,
            resolution.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("ticket #{id} not found"))?;
        Ok(format!("updated ticket #{id}"))
    }

    pub fn close(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CloseArgs = super::parse_args(args)?;
        let id = parsed.id;
        let reason = parsed.reason;
        ticketmd::update(
            &agent.cfg,
            &agent.workspace,
            id,
            &TicketEdit::default(),
            Some(TicketStatus::Closed),
            reason.as_deref(),
        )?
        .ok_or_else(|| anyhow::anyhow!("ticket #{id} not found"))?;
        Ok(format!("ticket #{id} closed"))
    }
}

type ToolGate = u8;
const GATE_FORMAL: ToolGate = 1 << 0;
const GATE_SKILLS: ToolGate = 1 << 1;

#[derive(Debug, Clone)]
pub struct Tool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    modes: &'static [Mode],
    gate: ToolGate,
    handler: fn(&mut Agent, &Value) -> Result<String>,
}

impl Tool {
    pub fn to_json(&self) -> Value {
        json!({
            "type": "function",
            "function": {
                "name": self.name,
                "description": self.description,
                "parameters": self.parameters,
            }
        })
    }

    fn available(&self, mode: Mode, formal: bool, has_skills: bool) -> bool {
        let capabilities =
            if formal { GATE_FORMAL } else { 0 } | if has_skills { GATE_SKILLS } else { 0 };
        self.modes.contains(&mode) && capabilities & self.gate == self.gate
    }
}

const ALL_MODES: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro];
const PLAN: &[Mode] = &[Mode::Plan];
#[cfg(feature = "formal")]
const PLAN_BUILD: &[Mode] = &[Mode::Plan, Mode::Build];
const PLAN_BUILD_EXPLORE: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore];
const RETRO: &[Mode] = &[Mode::Retro];

fn gated_tool(
    name: &'static str,
    modes: &'static [Mode],
    gate: ToolGate,
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        modes,
        gate,
        handler,
    }
}

fn tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    gated_tool(name, modes, 0, description, parameters, handler)
}

#[cfg(test)]
pub(crate) fn test_tool(name: &'static str, parameters: Value) -> Tool {
    tool(name, ALL_MODES, "d", parameters, basic::read)
}

#[cfg(feature = "formal")]
fn formal_tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    gated_tool(name, modes, GATE_FORMAL, description, parameters, handler)
}

fn skill_tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    gated_tool(name, modes, GATE_SKILLS, description, parameters, handler)
}

fn registry() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            tool("read", ALL_MODES, "Read a text file with line numbers. offset is 1-indexed.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"File path"},
                    "offset":{"type":"integer","description":"First line (1-indexed)"},
                    "limit":{"type":"integer","description":"Max lines to read (default 2000)"}
                },
                "required":["path"]
            }), basic::read),
            tool("write", ALL_MODES, "Create or overwrite a file, creating parent directories.", json!({
                "type":"object",
                "properties":{"path":{"type":"string"},"content":{"type":"string"}},
                "required":["path","content"]
            }), basic::write),
            tool("edit", ALL_MODES, "Apply precise text replacements to a file. Each oldText must match uniquely.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string"},
                    "edits":{"type":"array","items":{"type":"object","properties":{
                        "oldText":{"type":"string"},"newText":{"type":"string"}
                    },"required":["oldText","newText"]}},
                    "oldText":{"type":"string","description":"Single-edit shorthand"},
                    "newText":{"type":"string","description":"Single-edit shorthand"},
                    "replace_all":{"type":"boolean","description":"Allow replacing a non-unique oldText (single-edit form)"}
                },
                "required":["path"]
            }), basic::edit),
            tool("ls", ALL_MODES, "List files/directories respecting .gitignore.", json!({
                "type":"object",
                "properties":{
                    "path":{"type":"string","description":"Directory (default .)"},
                    "max_depth":{"type":"integer","description":"Recursion depth; Default = 0 = lists only immediate children"},
                    "show_hidden":{"type":"boolean","description":"default false"}
                }
            }), basic::ls),
            tool("bash", ALL_MODES, "Run a shell command via bash -c in the workspace. Returns exit code, stdout, stderr.", json!({
                "type":"object",
                "properties":{
                    "command":{"type":"string"},
                    "cwd":{"type":"string","description":"Working directory (default workspace)"},
                    "timeout_secs":{"type":"integer"}
                },
                "required":["command"]
            }), basic::bash),
            tool("plan_write", PLAN, "Persist an implementation plan as markdown under the plans directory (default .genji/plans/). Reuse the same title to refine an existing plan.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string","description":"Short plan title; drives the file name and default heading"},
                    "content":{"type":"string","description":"Plan body in markdown"},
                    "path":{"type":"string","description":"Optional explicit path (default: plans_dir/<title-slug>.md)"}
                },
                "required":["title","content"]
            }), plans::write),
            #[cfg(feature = "formal")]
            formal_tool("ticket_create", PLAN, "Create a work ticket.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":"integer"},
                    "requirement_id":{"type":"integer","description":"Requirement this ticket addresses"}
                },
                "required":["title"]
            }), tickets::create),
            #[cfg(feature = "formal")]
            formal_tool("ticket_read", PLAN_BUILD, "Read one ticket by id, or list actionable tickets.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "requirement_id":{"type":"integer"}
                }
            }), tickets::read),
            #[cfg(feature = "formal")]
            formal_tool("ticket_claim", PLAN_BUILD, "Claim the next open ticket (highest priority) or a specific ticket, marking it in_progress.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer","description":"Claim this ticket instead of the next one"},
                    "requirement_id":{"type":"integer","description":"Only consider tickets for this requirement"}
                }
            }), tickets::claim),
            #[cfg(feature = "formal")]
            formal_tool("ticket_update", PLAN_BUILD, "Update a ticket's fields and/or status.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":"integer"},
                    "requirement_id":{"type":"integer"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "resolution":{"type":"string"}
                },
                "required":["id"]
            }), tickets::update),
            #[cfg(feature = "formal")]
            formal_tool("ticket_close", PLAN_BUILD, "Close a ticket when its work is done and verified, or it is obsolete/duplicate/won't-fix.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"reason":{"type":"string"}},
                "required":["id"]
            }), tickets::close),
            #[cfg(feature = "formal")]
            formal_tool("requirement_create", PLAN, "Create a stakeholder or system requirement.", json!({
                "type":"object",
                "properties":{
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "parent_id":{"type":"integer","description":"Parent requirement id"}
                },
                "required":["level","title","body"]
            }), requirements::create),
            #[cfg(feature = "formal")]
            formal_tool("requirement_read", PLAN_BUILD, "Read a requirement by id, or list/filter requirements by level and status when id is omitted.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "status":{"type":"string","enum":["active","met","removed"]}
                }
            }), requirements::read),
            #[cfg(feature = "formal")]
            formal_tool("requirement_tree", PLAN_BUILD, "Show the requirement hierarchy with ticket coverage per requirement.", json!({
                "type":"object",
                "properties":{
                    "status":{"type":"string","enum":["active","met","removed"],"description":"Only show requirements with this status"}
                }
            }), requirements::tree),
            #[cfg(feature = "formal")]
            formal_tool("requirement_update", PLAN, "Update a requirement's title/body/status/level/parent.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "status":{"type":"string","enum":["active","met","removed"]},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "parent_id":{"type":"integer"}
                },
                "required":["id"]
            }), requirements::update),
            #[cfg(feature = "formal")]
            formal_tool("requirement_remove", PLAN, "Remove a requirement (soft by default).", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"hard":{"type":"boolean"}},
                "required":["id"]
            }), requirements::remove),
            #[cfg(feature = "formal")]
            formal_tool("requirement_ask", PLAN_BUILD, "Ask the user a clarifying question about a requirement. Recorded in the DB.", json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "requirement_id":{"type":"integer"}
                },
                "required":["question"]
            }), requirements::ask),
            skill_tool("skill_load", ALL_MODES, "Load a skill's instructions by name.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }), skills::load),
            tool("spawn", PLAN_BUILD_EXPLORE, "Spawn a subagent in a given mode that only reports back.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string","enum":["plan","build","explore"]},
                    "instructions":{"type":"string","description":"What the subagent should do"},
                    "task":{"type":"string","description":"Optional task label"}
                },
                "required":["mode","instructions"]
            }), spawn::spawn),
            tool("query_instances", RETRO, "List past agent instances.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string"},
                    "limit":{"type":"integer","description":"Default 20"}
                }
            }), retro::instances),
            tool("query_instance", RETRO, "Get all messages of one instance.", json!({
                "type":"object",
                "properties":{"instance_id":{"type":"string"},"limit":{"type":"integer"}},
                "required":["instance_id"]
            }), retro::instance),
            tool("query_messages", RETRO, "Search recorded messages by text/role/instance.", json!({
                "type":"object",
                "properties":{
                    "instance_id":{"type":"string"},
                    "role":{"type":"string"},
                    "search":{"type":"string"},
                    "limit":{"type":"integer","description":"Default 50"}
                }
            }), retro::messages),
            tool("query_tool_call", RETRO, "Query recorded tool calls (filter by name/errors/instance).", json!({
                "type":"object",
                "properties":{
                    "instance_id":{"type":"string"},
                    "name":{"type":"string"},
                    "errors_only":{"type":"boolean"},
                    "limit":{"type":"integer","description":"Default 50"}
                }
            }), retro::tool_calls),
            tool("query_stats", RETRO, "Aggregate stats: tool usage, error rates, skill loads, token usage.", json!({
                "type":"object","properties":{}
            }), retro::stats),
            tool("list_skills", RETRO, "List all skills with descriptions and use counts.", json!({
                "type":"object","properties":{}
            }), skills::list),
            tool("read_skill", RETRO, "Read a skill, optionally a specific version.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
                "required":["name"]
            }), skills::read),
            tool("write_skill", RETRO, "Create or overwrite a skill (versioned).", json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "description":{"type":"string"},
                    "content":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["name","content"]
            }), skills::write),
            tool("edit_skill", RETRO, "Edit a skill by text replacement (versioned).", json!({
                "type":"object",
                "properties":{
                    "name":{"type":"string"},
                    "oldText":{"type":"string"},
                    "newText":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["name","oldText","newText"]
            }), skills::edit),
            tool("skill_history", RETRO, "List versions of a skill.", json!({
                "type":"object","properties":{"name":{"type":"string"}},"required":["name"]
            }), skills::history),
            tool("skill_rollback", RETRO, "Activate an older version of a skill.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"},"version":{"type":"integer"}},
                "required":["name","version"]
            }), skills::rollback),
            tool("prompt_read", RETRO, "Read the active extended system prompt for a mode. RETRO has no extended prompt.", json!({
                "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
            }), retro::prompt_read),
            tool("prompt_edit", RETRO, "Replace the extended system prompt for a mode (versioned). Core prompt is not editable; RETRO has no extended prompt.", json!({
                "type":"object",
                "properties":{
                    "mode":{"type":"string","enum":["plan","build","explore"]},
                    "content":{"type":"string"},
                    "reason":{"type":"string"}
                },
                "required":["mode","content"]
            }), retro::prompt_edit),
            tool("prompt_history", RETRO, "List versions of a mode's extended prompt. RETRO has no extended prompt.", json!({
                "type":"object","properties":{"mode":{"type":"string","enum":["plan","build","explore"]}},"required":["mode"]
            }), retro::prompt_history),
            tool("prompt_rollback", RETRO, "Activate an older version of a mode's extended prompt. RETRO has no extended prompt.", json!({
                "type":"object",
                "properties":{"mode":{"type":"string","enum":["plan","build","explore"]},"version":{"type":"integer"}},
                "required":["mode","version"]
            }), retro::prompt_rollback),
        ]
    })
}

pub fn specs_for(mode: Mode, formal: bool, has_skills: bool) -> Vec<Tool> {
    registry()
        .iter()
        .filter(|t| t.available(mode, formal, has_skills))
        .cloned()
        .collect()
}

pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let res: Result<String> = (|| {
        let t = registry()
            .iter()
            .find(|t| t.name == name)
            .ok_or_else(|| anyhow!("unknown tool `{name}`"))?;
        let has_skills = t.gate & GATE_SKILLS == 0 || agent.db.has_skills()?;
        if !t.available(agent.mode, agent.formal, has_skills) {
            return Err(anyhow!("tool `{name}` is unavailable in the current mode"));
        }
        (t.handler)(agent, args)
    })();
    match res {
        Ok(s) => (bounded_result(&agent.cfg, &agent.workspace, name, s), false),
        Err(e) => (
            bounded_result(&agent.cfg, &agent.workspace, name, format!("ERROR: {e:#}")),
            true,
        ),
    }
}

fn bounded_result(cfg: &Config, workspace: &Path, name: &str, text: String) -> String {
    let max = cfg.tool_result_max_bytes;
    let total = text.len();
    if total <= max {
        return text;
    }
    match spill_to_log(cfg, workspace, name, &text) {
        Ok(path) => {
            let shown = path
                .strip_prefix(workspace)
                .unwrap_or(&path)
                .to_string_lossy();
            format!(
                "{}\n[full result ({total} bytes) written to {shown}; read it with the read tool]",
                crate::llm::truncate(&text, max),
            )
        }
        Err(_) => crate::llm::truncate(&text, max).into_owned(),
    }
}

static SPILL_SEQ: AtomicU64 = AtomicU64::new(0);

fn spill_to_log(cfg: &Config, workspace: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let dir = cfg.tmp_path(workspace);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let seq = SPILL_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let path = dir.join(format!("tool-{safe}-{ts}-{seq}.log"));
    std::fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Deserialize a tool payload once, so validation and handler inputs share one definition.
pub fn parse_args<T: DeserializeOwned>(args: &Value) -> Result<T> {
    serde_json::from_value(args.clone()).context("invalid tool arguments")
}

pub fn req_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
        .ok_or_else(|| anyhow!("missing required string argument `{key}`"))
}

pub fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(std::string::ToString::to_string)
}

pub fn opt_i64(args: &Value, key: &str) -> Option<i64> {
    args.get(key).and_then(Value::as_i64)
}

pub fn opt_bool(args: &Value, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

#[cfg(test)]
mod tests {
    use super::{bounded_result, specs_for};
    use crate::config::Config;
    use crate::storage::modes::Mode;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_workspace(tag: &str) -> std::path::PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!("genji-tools-{tag}-{nanos}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn small_results_are_returned_verbatim() {
        let ws = temp_workspace("small");
        let cfg = Config::default();
        assert_eq!(bounded_result(&cfg, &ws, "bash", "ok".into()), "ok");
        let _ = std::fs::remove_dir_all(&ws);
    }

    #[test]
    fn large_results_spill_to_tmp_log() {
        let ws = temp_workspace("spill");
        let cfg = Config {
            tool_result_max_bytes: 16,
            ..Default::default()
        };
        let full = "x".repeat(200);
        let out = bounded_result(&cfg, &ws, "bash", full.clone());
        assert!(out.contains("written to .genji/tmp/tool-bash-"), "{out}");
        assert!(out.contains("read it with the read tool"), "{out}");

        let name = out
            .split("written to ")
            .nth(1)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        let logged = std::fs::read_to_string(ws.join(name)).unwrap();
        assert_eq!(logged, full);
        let _ = std::fs::remove_dir_all(&ws);
    }

    fn names(mode: Mode, formal: bool) -> Vec<String> {
        specs_for(mode, formal, true)
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    #[test]
    fn tool_gate_requires_all_selected_capabilities() {
        let gated = super::gated_tool(
            "test",
            super::ALL_MODES,
            super::GATE_FORMAL | super::GATE_SKILLS,
            "test",
            serde_json::json!({}),
            super::basic::read,
        );
        assert!(!gated.available(Mode::Build, false, true));
        assert!(!gated.available(Mode::Build, true, false));
        assert!(gated.available(Mode::Build, true, true));
    }

    fn names_without_skills(mode: Mode, formal: bool) -> Vec<String> {
        specs_for(mode, formal, false)
            .into_iter()
            .map(|s| s.name.to_string())
            .collect()
    }

    fn is_ticket_or_requirement(name: &str) -> bool {
        name.starts_with("ticket_") || name.starts_with("requirement_")
    }

    #[test]
    fn plan_write_is_plan_only() {
        assert!(names(Mode::Plan, false).iter().any(|n| n == "plan_write"));
        for mode in [Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                !names(mode, false).iter().any(|n| n == "plan_write"),
                "{mode:?} exposed plan_write"
            );
        }
        assert!(names(Mode::Plan, true).iter().any(|n| n == "plan_write"));
    }

    #[test]
    fn ticket_tools_are_hidden_without_formal() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                names(mode, false)
                    .iter()
                    .all(|n| !is_ticket_or_requirement(n)),
                "{mode:?} exposed a ticket/requirement tool with Formal off"
            );
        }
    }

    #[cfg(feature = "formal")]
    #[test]
    fn ticket_tools_appear_with_formal() {
        let plan = names(Mode::Plan, true);
        assert!(plan.iter().any(|n| n == "ticket_create"));
        assert!(plan.iter().any(|n| n == "requirement_create"));
        assert!(plan.iter().any(|n| n == "requirement_tree"));

        let build = names(Mode::Build, true);
        assert!(build.iter().any(|n| n == "ticket_claim"));
        assert!(build.iter().any(|n| n == "ticket_update"));
        assert!(!build.iter().any(|n| n == "requirement_create"));
    }

    #[test]
    fn skill_load_hidden_without_skills() {
        for mode in [Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro] {
            assert!(
                !names_without_skills(mode, false)
                    .iter()
                    .any(|n| n == "skill_load"),
                "{mode:?} exposed skill_load with no skills"
            );
        }
    }

    #[test]
    fn formal_does_not_leak_into_explore_or_retro() {
        for mode in [Mode::Explore, Mode::Retro] {
            assert!(
                names(mode, true)
                    .iter()
                    .all(|n| !is_ticket_or_requirement(n)),
                "{mode:?} exposed a ticket/requirement tool with Formal on"
            );
        }
    }
}
