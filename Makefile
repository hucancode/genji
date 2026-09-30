B      := $(CURDIR)/benchmark
TASKS  := $(B)/tasks
JOBS   := $(B)/jobs
BENCH  := $(TASKS)/terminal-bench
HELLO  := $(TASKS)/hello-world/hello-world
LIGHT  := $(TASKS)/terminal-bench-swe-3
FULL   := $(TASKS)/terminal-bench-swe
STATIC := $(CURDIR)/target/x86_64-unknown-linux-gnu/release/genji

MODEL             ?= deepseek/deepseek-flash
MODE              ?= build
ARGS              ?=
N_CONCURRENT      ?=

# Harbor imports the custom agent (genji_agent:Genji) from here.
export PYTHONPATH := $(B):$(PYTHONPATH)

HARBOR = harbor run --jobs-dir $(JOBS) -a genji_agent:Genji -m $(MODEL)

.PHONY: hello light full status download build clean check-key

light: check-key
	@[ -d "$(BENCH)" ] || { echo "error: run 'make download' first"; exit 1; }
	@python3 $(B)/select_swe_tasks.py "$(BENCH)" "$(LIGHT)" --limit 3 --overwrite
	$(HARBOR) -p "$(LIGHT)" --n-concurrent $(or $(N_CONCURRENT),1)

full: check-key
	@[ -d "$(BENCH)" ] || { echo "error: run 'make download' first"; exit 1; }
	@python3 $(B)/select_swe_tasks.py "$(BENCH)" "$(FULL)" --overwrite
	$(HARBOR) -p "$(FULL)" --n-concurrent $(or $(N_CONCURRENT),1)

check-key:
	@[ -n "$(DEEPSEEK_API_KEY)" ] || { echo "error: set DEEPSEEK_API_KEY or create $(CURDIR)/genji-harbor-secret.txt"; exit 1; }

hello: check-key
	@[ -d "$(HELLO)" ] || harbor download hello-world@1.0 -o $(TASKS)
	$(HARBOR) -p "$(HELLO)" --n-concurrent $(or $(N_CONCURRENT),1)


status:
	@latest=$$(ls -1dt $(JOBS)/*/ 2>/dev/null | head -1); \
	if [ -z "$$latest" ]; then echo "no jobs in $(JOBS)"; exit 0; fi; \
	echo "job: $$latest"; \
	python3 -c "import json,glob,os,sys; \
j=sys.argv[1]; \
rows=[(os.path.basename(os.path.dirname(p)), (json.load(open(p)).get('verifier_result') or {}).get('rewards') or {}) for p in sorted(glob.glob(j+'/*/result.json'))]; \
print('%-44s %s' % ('trial', 'reward')); \
[print('%-44s %s' % (n, ','.join('%s=%s' % (k, v) for k, v in r.items()) or '-')) for n, r in rows]; \
print('%d finished trial(s)' % len(rows))" "$$latest"

download:
	harbor download terminal-bench/terminal-bench@4.0.0 -o $(TASKS)

build:
	cd $(CURDIR) && RUSTFLAGS="-C target-feature=+crt-static" \
	  cargo build --release --target x86_64-unknown-linux-gnu

clean:
	rm -rf $(JOBS)
