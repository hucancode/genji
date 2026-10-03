use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::agent::Agent;
use crate::config::Config;
use crate::storage::modes::Mode;
use crate::storage::util::{relative_path, tmp_file, write_file};

pub mod basic {
    use anyhow::{Context, Result, anyhow, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::fmt::Write as _;
    use std::time::Duration;

    use crate::agent::Agent;
    use crate::storage::proc;
    use crate::storage::util::write_file;

    #[derive(Debug, Deserialize)]
    struct ReadArgs {
        path: String,
        offset: Option<usize>,
        limit: Option<usize>,
    }

    #[derive(Debug, Deserialize)]
    struct WriteArgs {
        path: String,
        content: String,
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ReadArgs = super::parse_args(args)?;
        let path = agent.resolve_path(&parsed.path);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let offset = parsed.offset.unwrap_or(1).max(1);
        let limit = parsed.limit.unwrap_or(2000).max(1);

        let total = content.lines().count();
        if total == 0 {
            return Ok(format!("{} is empty (0 lines)", path.display()));
        }
        let start = (offset - 1).min(total);
        let end = (start + limit).min(total);
        let mut out = String::new();
        for (i, line) in content.lines().enumerate().take(end).skip(start) {
            writeln!(out, "{:>6}\t{line}", i + 1)?;
        }
        if end < total {
            writeln!(out, "\n[showing lines {}-{end} of {total}]", start + 1)?;
        }
        Ok(out)
    }

    pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: WriteArgs = super::parse_args(args)?;
        let path = agent.resolve_path(&parsed.path);
        write_file(&path, &parsed.content)?;
        Ok(format!(
            "wrote {} bytes to {}",
            parsed.content.len(),
            agent.display_path(&path)
        ))
    }

    /// Byte range of the one line window in `hay` equal to `needle` line by
    /// line, ignoring leading and trailing whitespace on each line.
    fn find_fuzzy(hay: &str, needle: &str) -> Option<(usize, usize)> {
        let want: Vec<&str> = needle.trim_matches('\n').lines().map(str::trim).collect();
        let mut lines = Vec::new();
        let mut at = 0;
        for l in hay.split_inclusive('\n') {
            let indent = l.len() - l.trim_start().len();
            let end = at + l.trim_end().len().max(indent);
            lines.push((at + indent, end, l.trim()));
            at += l.len();
        }
        let mut hits = lines
            .windows(want.len())
            .filter(|w| w.iter().map(|l| l.2).eq(want.iter().copied()));
        match (hits.next(), hits.next()) {
            (Some(w), None) => Some((w[0].0, w[w.len() - 1].1)),
            _ => None,
        }
    }

    fn find_unique(hay: &str, needle: &str) -> Result<(usize, usize)> {
        if needle.is_empty() {
            bail!("oldText must not be empty");
        }
        let mut it = hay.match_indices(needle);
        let shown = || needle.chars().take(60).collect::<String>();
        match (it.next(), it.next()) {
            (Some((i, _)), None) => Ok((i, i + needle.len())),
            (None, _) => {
                find_fuzzy(hay, needle).ok_or_else(|| anyhow!("oldText not found: {:?}", shown()))
            }
            (Some(_), Some(_)) => bail!("oldText is not unique: {:?}", shown()),
        }
    }

    fn apply_edits(content: &str, edits: &[(String, String)], replace_all: bool) -> Result<String> {
        if let [(old, new)] = edits
            && replace_all
        {
            if old.is_empty() {
                bail!("oldText must not be empty");
            }
            if !content.contains(old.as_str()) {
                bail!("oldText not found");
            }
            return Ok(content.replace(old.as_str(), new));
        }
        let mut ranges = Vec::new();
        for (old, new) in edits {
            let (start, end) = find_unique(content, old)?;
            ranges.push((start, end, new));
        }
        ranges.sort_by_key(|r| r.0);
        if ranges.windows(2).any(|w| w[0].1 > w[1].0) {
            bail!("edit ranges overlap");
        }
        let mut out = content.to_string();
        for (start, end, new) in ranges.into_iter().rev() {
            out.replace_range(start..end, new);
        }
        Ok(out)
    }

    #[derive(Debug, Deserialize)]
    struct EditArgs {
        path: String,
        edits: Option<Vec<EditEntry>>,
        #[serde(rename = "oldText", alias = "old_text")]
        old_text: Option<String>,
        #[serde(rename = "newText", alias = "new_text")]
        new_text: Option<String>,
        replace_all: Option<bool>,
    }
    #[derive(Debug, Deserialize)]
    struct EditEntry {
        #[serde(rename = "oldText", alias = "old_text")]
        old_text: String,
        #[serde(rename = "newText", alias = "new_text")]
        new_text: String,
    }

    pub fn edit(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: EditArgs = super::parse_args(args)?;
        let path = agent.resolve_path(&parsed.path);
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        let replace_all = parsed.replace_all.unwrap_or(false);
        let mut edits: Vec<(String, String)> = parsed
            .edits
            .unwrap_or_default()
            .into_iter()
            .map(|e| (e.old_text, e.new_text))
            .collect();
        if edits.is_empty()
            && let (Some(old), Some(new)) = (parsed.old_text, parsed.new_text)
        {
            edits.push((old, new));
        }
        if edits.is_empty() {
            bail!("no edits supplied (provide `edits` array or `oldText`/`newText`)");
        }

        let new_content = apply_edits(&content, &edits, replace_all)?;
        write_file(&path, &new_content)?;
        Ok(format!(
            "applied {} edit(s) to {} ({} -> {} bytes)",
            edits.len(),
            agent.display_path(&path),
            content.len(),
            new_content.len()
        ))
    }

    #[derive(Debug, Deserialize)]
    struct LsArgs {
        path: Option<String>,
        show_hidden: Option<bool>,
        max_depth: Option<usize>,
    }

    pub fn ls(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: LsArgs = super::parse_args(args)?;
        let rel = parsed.path.unwrap_or_else(|| ".".into());
        let root = agent.resolve_path(&rel);
        if !root.exists() {
            bail!("path does not exist: {}", root.display());
        }
        let show_hidden = parsed.show_hidden.unwrap_or(false);

        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .hidden(!show_hidden)
            .parents(false)
            .require_git(false)
            .sort_by_file_path(Ord::cmp)
            .max_depth(Some(parsed.max_depth.unwrap_or(0) + 1));

        let mut lines: Vec<String> = Vec::new();
        let mut dirs = 0usize;
        let mut files = 0usize;
        for entry in builder.build() {
            let Ok(entry) = entry else { continue };
            if entry.depth() == 0 {
                continue; // skip the root itself
            }
            let path = entry.path();
            let rel_path = agent.display_path(path);
            let Ok(meta) = entry.metadata() else { continue };
            if meta.is_dir() {
                dirs += 1;
                lines.push(format!("d        {rel_path}/"));
            } else {
                files += 1;
                lines.push(format!("f {:>8} {rel_path}", meta.len()));
            }
        }
        let mut out = lines.join("\n");
        if out.is_empty() {
            out = "(empty)".into();
        }
        let _ = write!(out, "\n\n[{dirs} dirs, {files} files under {rel}]");
        Ok(out)
    }

    #[derive(Debug, Deserialize)]
    struct BashArgs {
        command: String,
        cwd: Option<String>,
        timeout_secs: Option<i64>,
    }

    pub fn bash(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: BashArgs = super::parse_args(args)?;
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
        let res = proc::run_capture(
            "bash",
            &["-c".to_string(), command.clone()],
            &cwd,
            &agent.cfg.tmp_path(&agent.workspace),
            timeout,
            cap,
        )
        .with_context(|| format!("running command: {command}"))?;
        let mut out = format!(
            "exit_code: {}\n",
            res.code.map_or_else(|| "none".into(), |c| c.to_string())
        );
        if res.timed_out {
            let _ = writeln!(out, "[timed out after {}s]", timeout.as_secs());
        }
        for (label, text) in [("stdout", &res.stdout), ("stderr", &res.stderr)] {
            if !text.is_empty() {
                let _ = write!(out, "--- {label} ---\n{text}");
                if !text.ends_with('\n') {
                    out.push('\n');
                }
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
        fn whitespace_insensitive_fallback() {
            let src = "fn a() {\n    let x = 1;\n    let y = 2;\n}\n";
            let edits = [(
                "let x = 1;\nlet y = 2;".to_string(),
                "let z = 3;".to_string(),
            )];
            assert_eq!(
                apply_edits(src, &edits, false).unwrap(),
                "fn a() {\n    let z = 3;\n}\n"
            );
        }

        #[test]
        fn fuzzy_still_requires_uniqueness() {
            let src = "  a\n  b\n    a\n    b\n";
            assert!(apply_edits(src, &[("a\nb".to_string(), "c".to_string())], false).is_err());
        }

        #[test]
        fn missing_is_error() {
            assert!(apply_edits("abc", &[("zzz".to_string(), "x".to_string())], false).is_err());
        }
    }
}
pub mod plans {
    use anyhow::Result;
    use serde::Deserialize;
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    use super::parse_args;
    use crate::agent::Agent;
    use crate::config::Config;
    use crate::storage::util::{slugify, write_file};

    pub fn write_plan(
        cfg: &Config,
        workspace: &Path,
        title: &str,
        content: &str,
        path: Option<&str>,
    ) -> Result<PathBuf> {
        let path = match path {
            Some(p) => crate::storage::util::resolve_path(workspace, p),
            None => cfg.plan_file(workspace, &slugify(title, "plan")),
        };
        let body = if content.trim_start().starts_with("# ") {
            content.to_string()
        } else {
            format!("# {}\n\n{}", title.trim(), content.trim_start())
        };
        write_file(&path, &body)?;
        Ok(path)
    }

    #[derive(Debug, Deserialize)]
    struct WriteArgs {
        title: String,
        content: String,
        path: Option<String>,
    }

    pub fn write(agent: &mut Agent, args: &Value) -> Result<String> {
        let a: WriteArgs = parse_args(args)?;
        let path = write_plan(
            &agent.cfg,
            &agent.workspace,
            &a.title,
            &a.content,
            a.path.as_deref(),
        )?;
        Ok(format!("wrote plan to {}", agent.display_path(&path)))
    }

    #[cfg(test)]
    mod tests {
        use super::write_plan;
        use crate::config::Config;
        use crate::storage::util::temp_dir as temp_workspace;

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
    use std::fmt::Write as _;
    use std::path::Path;

    use crate::agent::Agent;
    use crate::config::Config;
    use crate::storage::reqmd::{self, Requirement, RequirementLevel, RequirementStatus};
    use crate::storage::util::FieldPatch;

    fn fmt_req(r: &Requirement, workspace: &Path, full: bool) -> String {
        let mut s = format!("#{} [{}:{}] {}\n", r.id, r.level, r.status, r.title);
        if let Some(p) = r.parent_id {
            let _ = writeln!(s, "parent: #{p}");
        }
        let _ = writeln!(s, "source: {}", r.source);
        let _ = writeln!(s, "source_path: {}", r.display_path(workspace));
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

    #[derive(Debug, Deserialize)]
    struct CreateArgs {
        level: RequirementLevel,
        title: String,
        body: String,
        parent_id: Option<i64>,
    }
    #[derive(Debug, Deserialize)]
    struct ReadArgs {
        id: Option<i64>,
        level: Option<RequirementLevel>,
        status: Option<RequirementStatus>,
    }
    #[derive(Debug, Deserialize)]
    struct UpdateArgs {
        id: i64,
        title: Option<String>,
        body: Option<String>,
        status: Option<RequirementStatus>,
        level: Option<RequirementLevel>,
        #[serde(default)]
        parent_id: FieldPatch<i64>,
    }
    #[derive(Debug, Deserialize)]
    struct RemoveArgs {
        id: i64,
    }
    #[derive(Debug, Deserialize)]
    struct TreeArgs {
        status: Option<RequirementStatus>,
    }
    #[derive(Debug, Deserialize)]
    struct AskArgs {
        question: String,
        requirement_id: Option<i64>,
    }

    pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CreateArgs = super::parse_args(args)?;
        let level = parsed.level;
        let r = reqmd::create(
            &agent.cfg,
            &agent.workspace,
            level,
            &parsed.title,
            &parsed.body,
            parsed.parent_id,
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
        read_requirements(
            &agent.cfg,
            &agent.workspace,
            parsed.id,
            parsed.level,
            parsed.status,
        )
    }

    pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: UpdateArgs = super::parse_args(args)?;
        let id = parsed.id;
        reqmd::update(&agent.cfg, &agent.workspace, id, |r| {
            if let Some(v) = parsed.title {
                r.title = v;
            }
            if let Some(v) = parsed.body {
                r.body = v;
            }
            if let Some(v) = parsed.status {
                r.status = v;
            }
            if let Some(v) = parsed.level {
                r.level = v;
            }
            parsed.parent_id.apply_to(&mut r.parent_id);
        })?
        .ok_or_else(|| anyhow::anyhow!("requirement #{id} not found"))?;
        Ok(format!("updated requirement #{id}"))
    }

    pub fn remove(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: RemoveArgs = super::parse_args(args)?;
        let id = parsed.id;
        if !reqmd::remove(&agent.cfg, &agent.workspace, id)? {
            bail!("requirement #{id} not found");
        }
        Ok(format!("deleted requirement #{id}"))
    }

    pub fn tree(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: TreeArgs = super::parse_args(args)?;
        let filter = parsed.status;
        let reqs: Vec<Requirement> = reqmd::load_all(&agent.cfg, &agent.workspace)?
            .into_iter()
            .filter(|r| filter.is_none_or(|s| r.status == s))
            .collect();
        if reqs.is_empty() {
            return Ok("(no requirements)".into());
        }

        let tickets = crate::storage::ticketmd::load_all(&agent.cfg, &agent.workspace)?;
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
        let _ = write!(
            out,
            "requirements: {} active / {} total, tickets: {} open / {} done\n\n",
            reqs.iter()
                .filter(|r| r.status == RequirementStatus::Active)
                .count(),
            reqs.len(),
            tickets.iter().filter(|t| t.status.is_open()).count(),
            tickets.iter().filter(|t| t.status.is_done()).count(),
        );
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
        let _ = writeln!(
            out,
            "{indent}#{} [{}:{}] {}  (tickets: {o} open / {d} done)",
            req.id, req.level, req.status, req.title
        );
        if let Some(kids) = children.get(&Some(req.id)) {
            for k in kids {
                render_tree(k, children, open, done, depth + 1, visited, out);
            }
        }
    }

    pub fn ask(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: AskArgs = super::parse_args(args)?;
        let question = parsed.question;
        let qid = agent
            .db
            .question_ask(parsed.requirement_id, &agent.instance_id, &question)?;
        Ok(format!(
            "recorded question #{qid}: {question}\n(no interactive user available; relay this question to the user)"
        ))
    }

    #[cfg(test)]
    mod tests {
        use super::read_requirements;
        use crate::config::Config;
        use crate::storage::reqmd::{self, RequirementLevel, RequirementStatus};
        use crate::storage::util::temp_dir as temp_workspace;

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
            reqmd::update(&cfg, &ws, child.id, |r| r.status = RequirementStatus::Met).unwrap();

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

            // A missing id is an error, not an empty list.
            assert!(read_requirements(&cfg, &ws, Some(999), None, None).is_err());

            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}
pub mod retro {
    use anyhow::Result;
    use rusqlite::{Params, Row, params};
    use serde::Deserialize;
    use serde_json::Value;
    use std::fmt::Write as _;

    use super::parse_args;
    use crate::agent::Agent;

    fn rows_text<P: Params>(
        agent: &Agent,
        sql: &str,
        p: P,
        empty: &str,
        fmt: impl FnMut(&Row) -> rusqlite::Result<String>,
    ) -> Result<String> {
        let mut stmt = agent.db.conn.prepare_cached(sql)?;
        let out = stmt
            .query_map(p, fmt)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .concat();
        Ok(if out.is_empty() { empty.into() } else { out })
    }

    #[derive(Deserialize)]
    struct InstancesArgs {
        mode: Option<String>,
        limit: Option<i64>,
    }
    #[derive(Deserialize)]
    struct MessagesArgs {
        instance_id: Option<String>,
        role: Option<String>,
        search: Option<String>,
        limit: Option<i64>,
    }
    #[derive(Deserialize)]
    struct ToolCallsArgs {
        instance_id: Option<String>,
        name: Option<String>,
        errors_only: Option<bool>,
        limit: Option<i64>,
    }

    pub fn instances(agent: &mut Agent, args: &Value) -> Result<String> {
        let a: InstancesArgs = parse_args(args)?;
        rows_text(
            agent,
            "SELECT id,mode,depth,status,tokens_used,started_at,substr(COALESCE(task,''),1,80) FROM instances WHERE (?1 IS NULL OR mode = ?1) ORDER BY started_at DESC LIMIT ?2",
            params![a.mode, a.limit.unwrap_or(20).clamp(1, 500)],
            "(no instances)",
            |r| {
                Ok(format!(
                    "{}\t{}\tdepth={}\t{}\ttokens={}\t{}\t{}\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?
                ))
            },
        )
    }

    /// The latest `limit` matching messages, oldest first, so one instance reads as a conversation.
    pub fn messages(agent: &mut Agent, args: &Value) -> Result<String> {
        let a: MessagesArgs = parse_args(args)?;
        let search = a.search.map(|q| format!("%{q}%"));
        rows_text(
            agent,
            "SELECT * FROM (SELECT id,instance_id,seq,role,created_at,substr(content,1,500),substr(tool_calls,1,200) FROM messages WHERE (?1 IS NULL OR instance_id = ?1) AND (?2 IS NULL OR role = ?2) AND (?3 IS NULL OR content LIKE ?3) ORDER BY id DESC LIMIT ?4) ORDER BY id",
            params![
                a.instance_id,
                a.role,
                search,
                a.limit.unwrap_or(50).clamp(1, 2000)
            ],
            "(no matching messages)",
            |r| {
                let mut s = format!(
                    "{}\t#{} [{}]\t{}\t{}\n",
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?
                );
                if let Some(tc) = r.get::<_, Option<String>>(6)? {
                    let _ = writeln!(s, "    tool_calls: {tc}");
                }
                Ok(s)
            },
        )
    }

    pub fn tool_calls(agent: &mut Agent, args: &Value) -> Result<String> {
        let a: ToolCallsArgs = parse_args(args)?;
        rows_text(
            agent,
            "SELECT instance_id,name,is_error,duration_ms,substr(args,1,160),substr(result,1,300),created_at FROM tool_calls WHERE (?1 IS NULL OR instance_id = ?1) AND (?2 IS NULL OR name = ?2) AND (?3 = 0 OR is_error = 1) ORDER BY id DESC LIMIT ?4",
            params![
                a.instance_id,
                a.name,
                a.errors_only.unwrap_or(false),
                a.limit.unwrap_or(50).clamp(1, 500)
            ],
            "(no matching tool calls)",
            |r| {
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
            },
        )
    }

    pub fn stats(agent: &mut Agent, _args: &Value) -> Result<String> {
        let (instances, tokens, compactions): (i64, i64, i64) = agent.db.conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(tokens_used),0), (SELECT COUNT(*) FROM compactions) FROM instances",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let by_mode = rows_text(
            agent,
            "SELECT mode,COUNT(*) FROM instances GROUP BY mode ORDER BY 2 DESC",
            [],
            "  (none)\n",
            |r| {
                Ok(format!(
                    "  {}: {}\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?
                ))
            },
        )?;
        let tools = rows_text(
            agent,
            "SELECT name,COUNT(*),COALESCE(SUM(is_error),0),CAST(AVG(duration_ms) AS INT) FROM tool_calls GROUP BY name ORDER BY 2 DESC",
            [],
            "  (none)\n",
            |r| {
                Ok(format!(
                    "  {}: calls={} errors={} avg={}ms\n",
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, i64>(3)?
                ))
            },
        )?;
        let skills = rows_text(
            agent,
            "SELECT json_extract(args,'$.name'),COUNT(*) FROM tool_calls WHERE name='skill_load' GROUP BY 1 ORDER BY 2 DESC LIMIT 20",
            [],
            "  (none)\n",
            |r| {
                Ok(format!(
                    "  {}: {}\n",
                    r.get::<_, Option<String>>(0)?.unwrap_or_default(),
                    r.get::<_, i64>(1)?
                ))
            },
        )?;
        Ok(format!(
            "instances: {instances}, total tokens: {tokens}\ninstances by mode:\n{by_mode}tool calls (name, count, errors, avg_ms):\n{tools}skills by loads:\n{skills}compactions: {compactions}\n"
        ))
    }
}
pub mod skills {
    use anyhow::{Result, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::path::PathBuf;

    use super::parse_args;
    use crate::agent::Agent;
    use crate::config::Config;
    use crate::storage::util::{split_frontmatter, valid_slug};

    fn skill_files(cfg: &Config, workspace: &std::path::Path) -> impl Iterator<Item = PathBuf> {
        std::fs::read_dir(cfg.skills_path(workspace))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
    }

    fn names(cfg: &Config, workspace: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = skill_files(cfg, workspace)
            .filter_map(|p| p.file_stem().and_then(|s| s.to_str()).map(str::to_owned))
            .collect();
        names.sort();
        names
    }

    pub fn any(cfg: &Config, workspace: &std::path::Path) -> bool {
        skill_files(cfg, workspace).next().is_some()
    }

    #[derive(Deserialize)]
    struct LoadArgs {
        name: String,
    }

    pub fn load(agent: &mut Agent, args: &Value) -> Result<String> {
        let LoadArgs { name } = parse_args(args)?;
        let path = agent
            .cfg
            .skills_path(&agent.workspace)
            .join(format!("{name}.md"));
        let text = match valid_slug(&name).then(|| std::fs::read_to_string(&path)) {
            Some(Ok(text)) => text,
            _ => bail!(
                "skill `{name}` not found. available: {}",
                names(&agent.cfg, &agent.workspace).join(", ")
            ),
        };
        let (meta, body) = split_frontmatter(&text);
        let description = meta.get("description").map_or("", String::as_str);
        Ok(format!("# Skill: {name}\n{description}\n\n{body}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn lists_markdown_files_only() {
            let ws = crate::storage::util::temp_dir("skills");
            let cfg = Config::default();
            let dir = cfg.skills_path(&ws);
            std::fs::create_dir_all(&dir).unwrap();
            assert!(!any(&cfg, &ws));
            std::fs::write(dir.join("b.md"), "x").unwrap();
            std::fs::write(dir.join("a.md"), "x").unwrap();
            std::fs::write(dir.join("c.txt"), "x").unwrap();
            assert_eq!(names(&cfg, &ws), ["a", "b"]);
            let _ = std::fs::remove_dir_all(&ws);
        }
    }
}
pub mod spawn {
    use anyhow::{Result, bail};
    use serde::Deserialize;
    use serde_json::{Value, json};
    use std::path::Path;
    use std::time::Duration;

    use super::parse_args;
    use crate::agent::Agent;
    use crate::storage::proc;
    use crate::storage::util::{TempPath, tmp_file, write_file};

    #[derive(Deserialize)]
    struct SpawnArgs {
        mode: String,
        instructions: String,
        task: Option<String>,
    }

    pub fn spawn(agent: &mut Agent, args: &Value) -> Result<String> {
        let a: SpawnArgs = parse_args(args)?;
        if !["plan", "build", "explore"].contains(&a.mode.as_str()) {
            bail!("spawn mode must be plan|build|explore (not retro)");
        }
        if agent.depth >= agent.cfg.max_subagent_depth {
            bail!(
                "subagent depth limit reached ({} >= {})",
                agent.depth,
                agent.cfg.max_subagent_depth
            );
        }
        let task = a.task.unwrap_or_else(|| format!("subagent:{}", a.mode));
        let tmp = agent.cfg.tmp_path(&agent.workspace);
        let exe = std::env::current_exe().unwrap_or_else(|_| "genji".into());
        let instruction_file = TempPath(tmp_file(&tmp, "subagent", "md"));
        write_file(&instruction_file.0, &a.instructions)?;
        let cmd_args = build_subagent_args(
            &a.mode,
            &agent.instance_id,
            &instruction_file.0,
            &task,
            agent.depth,
            agent.formal,
        );
        // The child's stdout is a JSONL event stream whose last line carries the
        // report, so it must be read whole; only the parsed report is bounded.
        let res = proc::run_capture(
            &exe.to_string_lossy(),
            &cmd_args,
            &agent.workspace,
            &tmp,
            Duration::from_secs(agent.cfg.spawn_timeout_secs),
            usize::MAX,
        )?;

        // The child streams JSON events on stdout; only its identity and final report matter here.
        let mut sub_instance = None;
        let mut end = None;
        for line in res.stdout.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            match event["type"].as_str() {
                Some("instance_start") => {
                    sub_instance = event["instance"].as_str().map(String::from)
                }
                Some("instance_end") => end = Some(event),
                _ => {}
            }
        }
        let (mut status, report) = match &end {
            Some(e) => (
                e["status"].as_str().unwrap_or("unknown"),
                e["report"].as_str().unwrap_or("").to_string(),
            ),
            None => {
                let text = if res.stdout.trim().is_empty() {
                    &res.stderr
                } else {
                    &res.stdout
                };
                ("incomplete", text.trim().to_string())
            }
        };
        if res.timed_out {
            status = "timed_out";
        }
        Ok(json!({
            "subagent_instance": sub_instance,
            "mode": a.mode,
            "status": status,
            "exit_code": res.code,
            "timed_out": res.timed_out,
            "duration_ms": res.duration_ms,
            "report": crate::llm::truncate(&report, agent.cfg.tool_result_max_bytes.saturating_sub(1024).max(4096)),
        })
        .to_string())
    }

    fn build_subagent_args(
        mode: &str,
        parent_instance: &str,
        instructions: &Path,
        task: &str,
        depth: u32,
        formal: bool,
    ) -> Vec<String> {
        let mut args: Vec<String> = [
            mode,
            "--subagent",
            "--parent-instance",
            parent_instance,
            "--instructions-file",
            &instructions.to_string_lossy(),
            "--label",
            task,
            "--depth",
            &(depth + 1).to_string(),
            "--quiet-startup",
            "--no-control",
        ]
        .map(String::from)
        .into();
        // Subagents inherit Formal so a build subagent can work the same tickets.
        if formal {
            args.push("--formal".to_string());
        }
        args
    }
}
#[cfg(feature = "formal")]
pub mod tickets {
    use anyhow::{Result, bail};
    use serde::Deserialize;
    use serde_json::Value;
    use std::fmt::Write as _;
    use std::path::Path;

    use crate::agent::Agent;
    use crate::storage::ticketmd::{self, Ticket, TicketStatus};
    use crate::storage::util::FieldPatch;

    fn fmt_ticket(t: &Ticket, workspace: &Path) -> String {
        let mut s = format!("#{} [{}] {}\n", t.id, t.status, t.title);
        if t.priority != 2 {
            let _ = writeln!(s, "priority: {}", t.priority);
        }
        if let Some(r) = t.requirement_id {
            let _ = writeln!(s, "requirement: #{r}");
        }
        if let Some(p) = t.parent_id {
            let _ = writeln!(s, "parent: #{p}");
        }
        let _ = writeln!(s, "path: {}", t.display_path(workspace));
        let _ = writeln!(s, "created: {}", t.created_at);
        if let Some(r) = &t.resolution {
            let _ = writeln!(s, "resolution: {r}");
        }
        if !t.description.trim().is_empty() {
            s.push('\n');
            s.push_str(t.description.trim());
            s.push('\n');
        }
        s
    }

    #[derive(Debug, Deserialize)]
    struct CreateArgs {
        title: String,
        description: Option<String>,
        priority: Option<i64>,
        parent_id: Option<i64>,
        requirement_id: Option<i64>,
    }
    #[derive(Debug, Deserialize)]
    struct ReadArgs {
        id: Option<i64>,
        status: Option<TicketStatus>,
        requirement_id: Option<i64>,
    }
    #[derive(Debug, Deserialize)]
    struct ClaimArgs {
        id: Option<i64>,
        requirement_id: Option<i64>,
    }
    #[derive(Debug, Deserialize)]
    struct UpdateArgs {
        id: i64,
        title: Option<String>,
        description: Option<String>,
        priority: Option<i64>,
        #[serde(default)]
        parent_id: FieldPatch<i64>,
        #[serde(default)]
        requirement_id: FieldPatch<i64>,
        status: Option<TicketStatus>,
        resolution: Option<String>,
    }
    #[derive(Debug, Deserialize)]
    struct CloseArgs {
        id: i64,
        reason: Option<String>,
    }

    pub fn create(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CreateArgs = super::parse_args(args)?;
        let t = ticketmd::create(
            &agent.cfg,
            &agent.workspace,
            &parsed.title,
            parsed.description.as_deref().unwrap_or_default(),
            parsed.priority.unwrap_or(2).clamp(1, 3),
            parsed.parent_id,
            parsed.requirement_id,
            agent.mode.as_str(),
        )?;
        Ok(format!("created ticket #{}: {}", t.id, t.title))
    }

    pub fn read(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: ReadArgs = super::parse_args(args)?;
        if let Some(id) = parsed.id {
            return match ticketmd::load_by_id(&agent.cfg, &agent.workspace, id)? {
                Some(t) => Ok(fmt_ticket(&t, &agent.workspace)),
                None => bail!("ticket #{id} not found"),
            };
        }
        let status = parsed.status;
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
        let (id, requirement_id) = (parsed.id, parsed.requirement_id);
        let ticket = match id {
            Some(id) => ticketmd::load_by_id(&agent.cfg, &agent.workspace, id)?,
            None => ticketmd::load_all(&agent.cfg, &agent.workspace)?
                .into_iter()
                .find(|t| {
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
        let Some(t) = ticketmd::update(&agent.cfg, &agent.workspace, t.id, |t| {
            t.status = TicketStatus::InProgress;
        })?
        else {
            bail!("ticket #{} is archived and cannot be claimed", t.id);
        };
        Ok(format!(
            "claimed ticket #{}\n\n{}",
            t.id,
            fmt_ticket(&t, &agent.workspace)
        ))
    }

    pub fn update(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: UpdateArgs = super::parse_args(args)?;
        let id = parsed.id;
        ticketmd::update(&agent.cfg, &agent.workspace, id, |t| {
            if let Some(v) = parsed.title {
                t.title = v;
            }
            if let Some(v) = parsed.description {
                t.description = v;
            }
            if let Some(v) = parsed.priority {
                t.priority = v;
            }
            parsed.parent_id.apply_to(&mut t.parent_id);
            parsed.requirement_id.apply_to(&mut t.requirement_id);
            if let Some(v) = parsed.status {
                t.status = v;
            }
            if let Some(v) = parsed.resolution {
                t.resolution = Some(v);
            }
        })?
        .ok_or_else(|| anyhow::anyhow!("ticket #{id} not found"))?;
        Ok(format!("updated ticket #{id}"))
    }

    pub fn close(agent: &mut Agent, args: &Value) -> Result<String> {
        let parsed: CloseArgs = super::parse_args(args)?;
        let id = parsed.id;
        ticketmd::update(&agent.cfg, &agent.workspace, id, |t| {
            t.status = TicketStatus::Closed;
            if let Some(v) = parsed.reason {
                t.resolution = Some(v);
            }
        })?
        .ok_or_else(|| anyhow::anyhow!("ticket #{id} not found"))?;
        Ok(format!("ticket #{id} closed"))
    }
}

/// Optional capability a tool needs before it is offered to the model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Gate {
    None,
    #[cfg_attr(not(feature = "formal"), allow(dead_code))]
    Formal,
    Skills,
}

struct Tool {
    name: &'static str,
    description: &'static str,
    parameters: Value,
    modes: &'static [Mode],
    gate: Gate,
    handler: fn(&mut Agent, &Value) -> Result<String>,
}

impl Tool {
    fn gate(mut self, gate: Gate) -> Self {
        self.gate = gate;
        self
    }

    fn to_json(&self) -> Value {
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
        self.modes.contains(&mode)
            && match self.gate {
                Gate::None => true,
                Gate::Formal => formal,
                Gate::Skills => has_skills,
            }
    }
}

const ALL_MODES: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore, Mode::Retro];
const PLAN: &[Mode] = &[Mode::Plan];
#[cfg(feature = "formal")]
const PLAN_BUILD: &[Mode] = &[Mode::Plan, Mode::Build];
const PLAN_BUILD_EXPLORE: &[Mode] = &[Mode::Plan, Mode::Build, Mode::Explore];
const RETRO: &[Mode] = &[Mode::Retro];

fn tool(
    name: &'static str,
    modes: &'static [Mode],
    description: &'static str,
    parameters: Value,
    handler: fn(&mut Agent, &Value) -> Result<String>,
) -> Tool {
    Tool {
        name,
        description,
        parameters,
        modes,
        gate: Gate::None,
        handler,
    }
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
            tool("ticket_create", PLAN, "Create a work ticket.", json!({
                "type":"object",
                "properties":{
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":"integer"},
                    "requirement_id":{"type":"integer","description":"Requirement this ticket addresses"}
                },
                "required":["title"]
            }), tickets::create).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("ticket_read", PLAN_BUILD, "Read one ticket by id, or list actionable tickets.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "requirement_id":{"type":"integer"}
                }
            }), tickets::read).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("ticket_claim", PLAN_BUILD, "Claim the next open ticket (highest priority) or a specific ticket, marking it in_progress.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer","description":"Claim this ticket instead of the next one"},
                    "requirement_id":{"type":"integer","description":"Only consider tickets for this requirement"}
                }
            }), tickets::claim).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("ticket_update", PLAN_BUILD, "Update a ticket's fields and/or status.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "description":{"type":"string"},
                    "priority":{"type":"integer","description":"1=high, 2=normal, 3=low"},
                    "parent_id":{"type":["integer","null"],"description":"null clears it"},
                    "requirement_id":{"type":["integer","null"],"description":"null clears it"},
                    "status":{"type":"string","enum":["open","in_progress","resolved","closed"]},
                    "resolution":{"type":"string"}
                },
                "required":["id"]
            }), tickets::update).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("ticket_close", PLAN_BUILD, "Close a ticket when its work is done and verified, or it is obsolete/duplicate/won't-fix.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"},"reason":{"type":"string"}},
                "required":["id"]
            }), tickets::close).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_create", PLAN, "Create a stakeholder or system requirement.", json!({
                "type":"object",
                "properties":{
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "parent_id":{"type":"integer"}
                },
                "required":["level","title","body"]
            }), requirements::create).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_read", PLAN_BUILD, "Read a requirement by id, or list/filter requirements by level and status when id is omitted.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "status":{"type":"string","enum":["active","met"]}
                }
            }), requirements::read).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_tree", PLAN_BUILD, "Show the requirement hierarchy with ticket coverage per requirement.", json!({
                "type":"object",
                "properties":{
                    "status":{"type":"string","enum":["active","met"],"description":"Only show requirements with this status"}
                }
            }), requirements::tree).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_update", PLAN, "Update a requirement's title/body/status/level/parent.", json!({
                "type":"object",
                "properties":{
                    "id":{"type":"integer"},
                    "title":{"type":"string"},
                    "body":{"type":"string"},
                    "status":{"type":"string","enum":["active","met"]},
                    "level":{"type":"string","enum":["stakeholder","system"]},
                    "parent_id":{"type":["integer","null"],"description":"null clears it"}
                },
                "required":["id"]
            }), requirements::update).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_remove", PLAN, "Remove a requirement.", json!({
                "type":"object",
                "properties":{"id":{"type":"integer"}},
                "required":["id"]
            }), requirements::remove).gate(Gate::Formal),
            #[cfg(feature = "formal")]
            tool("requirement_ask", PLAN, "Raise a formal question about an ambiguous requirement for a human to resolve. Recorded in the DB.", json!({
                "type":"object",
                "properties":{
                    "question":{"type":"string"},
                    "requirement_id":{"type":"integer"}
                },
                "required":["question"]
            }), requirements::ask).gate(Gate::Formal),
            tool("skill_load", ALL_MODES, "Load a skill's instructions by name.", json!({
                "type":"object",
                "properties":{"name":{"type":"string"}},
                "required":["name"]
            }), skills::load).gate(Gate::Skills),
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
            tool("query_messages", RETRO, "Read recorded messages (latest N, oldest first). Filter by instance_id to read one conversation; add role/search to narrow.", json!({
                "type":"object",
                "properties":{
                    "instance_id":{"type":"string"},
                    "role":{"type":"string"},
                    "search":{"type":"string"},
                    "limit":{"type":"integer","description":"Default 50, max 2000"}
                }
            }), retro::messages),
            tool("query_tool_calls", RETRO, "Query recorded tool calls (filter by name/errors/instance).", json!({
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
        ]
    })
}

pub fn specs_for(mode: Mode, formal: bool, has_skills: bool) -> Vec<Value> {
    registry()
        .iter()
        .filter(|t| t.available(mode, formal, has_skills))
        .map(Tool::to_json)
        .collect()
}

fn run_tool(agent: &mut Agent, name: &str, args: &Value) -> Result<String> {
    let t = registry()
        .iter()
        .find(|t| t.name == name)
        .ok_or_else(|| anyhow!("unknown tool `{name}`"))?;
    // `skill_load` reports missing skills itself, so skills are not re-scanned here.
    if !t.available(agent.mode, agent.formal, true) {
        bail!("tool `{name}` is unavailable in the current mode");
    }
    (t.handler)(agent, args)
}

pub fn dispatch(agent: &mut Agent, name: &str, args: &Value) -> (String, bool) {
    let (text, is_error) = match run_tool(agent, name, args) {
        Ok(s) => (s, false),
        Err(e) => (format!("ERROR: {e:#}"), true),
    };
    (
        bounded_result(&agent.cfg, &agent.workspace, name, text),
        is_error,
    )
}

fn bounded_result(cfg: &Config, workspace: &Path, name: &str, text: String) -> String {
    let max = cfg.tool_result_max_bytes;
    if text.len() <= max {
        return text;
    }
    let clipped = crate::llm::truncate(&text, max);
    match spill_to_log(cfg, workspace, name, &text) {
        Ok(path) => format!(
            "{clipped}\n[full result ({} bytes) written to {}; read it with the read tool]",
            text.len(),
            relative_path(workspace, &path),
        ),
        Err(_) => clipped.into_owned(),
    }
}

fn spill_to_log(cfg: &Config, workspace: &Path, name: &str, text: &str) -> Result<PathBuf> {
    let safe: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        .collect();
    let path = tmp_file(&cfg.tmp_path(workspace), &format!("tool-{safe}"), "log");
    write_file(&path, text)?;
    Ok(path)
}

/// Deserialize a tool payload, so validation and handler inputs share one definition.
pub fn parse_args<'a, T: Deserialize<'a>>(args: &'a Value) -> Result<T> {
    T::deserialize(args).context("invalid tool arguments")
}

#[cfg(test)]
mod tests {
    use super::{bounded_result, specs_for};
    use crate::config::Config;
    use crate::storage::modes::Mode;
    use crate::storage::util::temp_dir as temp_workspace;

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
        names_with(mode, formal, true)
    }

    fn names_with(mode: Mode, formal: bool, skills: bool) -> Vec<String> {
        specs_for(mode, formal, skills)
            .into_iter()
            .map(|s| s["function"]["name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn tool_gate_requires_its_capability() {
        let gated = super::tool(
            "test",
            super::ALL_MODES,
            "test",
            serde_json::json!({}),
            super::basic::read,
        )
        .gate(super::Gate::Skills);
        assert!(!gated.available(Mode::Build, true, false));
        assert!(gated.available(Mode::Build, false, true));
        let formal = super::tool(
            "t",
            super::PLAN,
            "t",
            serde_json::json!({}),
            super::basic::read,
        )
        .gate(super::Gate::Formal);
        assert!(!formal.available(Mode::Plan, false, true));
        assert!(formal.available(Mode::Plan, true, false));
        assert!(!formal.available(Mode::Build, true, true));
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
                !names_with(mode, false, false)
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
