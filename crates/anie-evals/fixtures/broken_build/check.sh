#!/bin/sh
# Build stand-in for the verify_broken_fixture scenario: "compiles"
# only once greeting.txt says exactly "hello world". Cheap and
# dependency-free so the harness verify command can run it on every
# edit without slowing the eval.
if grep -qx "hello world" greeting.txt; then
    echo "build OK"
else
    echo "build FAILED: greeting.txt line 1 must be exactly 'hello world'" >&2
    exit 1
fi
