#!/bin/bash
. /tests/lib.sh
file_eq /app/answer.txt a3f9c2e1
changed_only answer.txt
pass
