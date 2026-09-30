This directory
contains a [Harbor](https://www.harborframework.com/) **agent adapter** 
for genji plus scripts to run the **software-engineering subset of `terminal-bench/terminal-bench@4.0.0`**.

```bash
genji_agent.py         # the Harbor agent
select_swe_tasks.py    # filter a dataset to [metadata].category == "Software"
tasks/                 # downloaded datasets + generated SWE subsets
jobs/                  # job output / results
```
