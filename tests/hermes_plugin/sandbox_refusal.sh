#!/bin/sh
# Inside real_hermes_sandbox.sh only: test_real_hermes.py must refuse a Hermes tree it could write.
mkdir -p /work/h/.hermes/hermes-agent /work/h/.hermes/installs /work/h/.hermes/tools
HOME=/work/h /usr/bin/python3 /tests/test_real_hermes.py
status=$?
if [ "$status" -eq 3 ]; then
    echo "REFUSAL OK"
    exit 0
fi
echo "REFUSAL MISSING: exit $status" >&2
exit 1
