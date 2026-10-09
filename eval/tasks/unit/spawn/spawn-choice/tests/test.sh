#!/bin/bash
# The reward is 1 when test.py exits 0; its traceback names the failed check.
mkdir -p /logs/verifier
if python3 /tests/test.py; then r=1; else r=0; fi
echo $r > /logs/verifier/reward.txt
