#!/bin/sh
# Inside real_hermes_sandbox.sh only: test_real_hermes.py must refuse a Hermes tree it could write,
# and the sandbox's own Hermes home once this run's token is gone.
mkdir -p /work/h/.hermes/hermes-agent /work/h/.hermes/installs /work/h/.hermes/tools
HOME=/work/h /usr/bin/python3 /tests/test_real_hermes.py
status=$?
if [ "$status" -ne 3 ]; then
    echo "REFUSAL MISSING: exit $status" >&2
    exit 1
fi
env -u HERDR_AGENT_QUOTA_SANDBOX_HOME_TOKEN /usr/bin/python3 /tests/test_real_hermes.py
status=$?
if [ "$status" -ne 3 ]; then
    echo "TOKEN REFUSAL MISSING: exit $status" >&2
    exit 1
fi
echo "REFUSAL OK"
