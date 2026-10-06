#!/bin/bash
grep -m1 ' ERROR ' /app/logs/service.log | sed 's/.*req=\([^ ]*\).*/\1/' > /app/answer.txt
