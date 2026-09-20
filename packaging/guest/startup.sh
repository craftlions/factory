#!/bin/sh
# Runs inside a session microvm before the harness starts. Its output is shown
# in the session's chat. The guest has no network device: every download goes
# through the factory's proxy, which only allows the hosts mise needs here.
set -eu

echo "Installing tools with mise"
mise install
mise ls --current
