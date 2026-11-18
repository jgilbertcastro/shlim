#!/usr/bin/env bash
# Compares what an agent receives from a plain shell with what it receives through shlim,
# for common noisy commands. Everything happens in a temporary directory.
#
#   scripts/bench.sh                       run every scenario whose tool is installed
#   scripts/bench.sh npm cargo             run only these
#
# Scenarios: dotnet-build dotnet-test npm pnpm cargo docker
# Needs: shlim on PATH (or: PATH=target/release:$PATH scripts/bench.sh), python3, network for
# the first package download. A scenario is skipped when its tool is missing or its setup fails.
#
# Method: each command runs once to warm package caches, then twice with the state reset:
# once as a plain shell would (no TTY, stdout+stderr), once through `shlim mcp`, playing the
# agent: `run`, then `read wait=exit` until the process ends. The shlim figure is the total
# size of every reply. Tokens are estimated as bytes/4. One run per scenario: treat the numbers
# as indicative, not as a statistic.
set -u

command -v shlim >/dev/null || { echo "shlim is not on PATH" >&2; exit 1; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 1; }

R="$(mktemp -d /tmp/shlim-bench.XXXXXX)"
export SHLIM_HOME="$(mktemp -d /tmp/shb.XXXXXX)"
trap 'shlim stop >/dev/null 2>&1; rm -rf "$R" "$SHLIM_HOME"' EXIT

cat > "$R/agent.py" <<'PY'
import json, subprocess, sys
# Plays the agent. Prints "<total bytes of replies> <number of replies>".
p = subprocess.Popen(["shlim", "mcp"], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
def send(msg):
    p.stdin.write(json.dumps(msg) + "\n"); p.stdin.flush()
    return json.loads(p.stdout.readline())
send({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {}})
def call(i, name, args):
    return send({"jsonrpc": "2.0", "id": i, "method": "tools/call", "params": {"name": name, "arguments": args}})["result"]["content"][0]["text"]
out = call(1, "run", {"cmd": sys.argv[1], "timeout": 10})
replies = [out]
for n in range(2, 12):
    head = out.split("\n")[0]
    if not (head.startswith("[#") and ("running" in head or "timeout" in head or "waiting" in head)): break
    sid = int(head.split("#")[1].split(" ")[0].rstrip("]"))
    out = call(n, "read", {"id": sid, "wait": "exit", "timeout": 300})
    replies.append(out)
print(sum(map(len, replies)), len(replies))
PY

ROWS=()
measure() { # name dir command reset-command
  local name=$1 dir=$2 cmd=$3 reset=$4
  echo "… $name" >&2
  ( cd "$dir" && eval "$reset" >/dev/null 2>&1; eval "$cmd" >/dev/null 2>&1 )   # warm caches
  ( cd "$dir" && eval "$reset" >/dev/null 2>&1 )
  local plain; plain=$( ( cd "$dir" && eval "$cmd" 2>&1 ) | wc -c | tr -d ' ' )
  ( cd "$dir" && eval "$reset" >/dev/null 2>&1 )
  local res; res=$(python3 "$R/agent.py" "cd '$dir' && $cmd")
  shlim stop >/dev/null 2>&1
  local sent=${res%% *} replies=${res##* }
  local pct=0; [ "$plain" -gt 0 ] && pct=$(( (plain - sent) * 100 / plain ))
  ROWS+=("$(printf '%-34s %9s %9s %6s%%  %s' "$name" "$plain" "$sent" "$pct" "$replies")")
}

want() { [ $# -eq 0 ] && return 0; for s in "${SELECT[@]}"; do [ "$s" = "$1" ] && return 0; done; return 1; }
SELECT=("$@")
pick() { [ ${#SELECT[@]} -eq 0 ] || want "$1"; }

if pick dotnet-build && command -v dotnet >/dev/null; then
  dotnet new console -o "$R/dnwarn" >/dev/null 2>&1 && \
  python3 -c "open('$R/dnwarn/Program.cs','w').write('\n'.join(f'int unused{i} = {i};' for i in range(80)) + '\nSystem.Console.WriteLine(1);\n')" && \
  measure "dotnet build (80 warnings)" "$R/dnwarn" "dotnet build" "rm -rf bin obj"
fi
if pick dotnet-test && command -v dotnet >/dev/null; then
  dotnet new xunit -o "$R/dntest" >/dev/null 2>&1 && \
  python3 -c "
rows='\n'.join(f'    [InlineData({i})]' for i in range(60))
open('$R/dntest/UnitTest1.cs','w').write('using Xunit;\nnamespace T;\npublic class UnitTest1 {\n  [Theory]\n'+rows+'\n  public void Positive(int n) { Assert.True(n >= 0); }\n}\n')" && \
  measure "dotnet test (60 tests, normal)" "$R/dntest" "dotnet test --logger 'console;verbosity=normal'" "rm -rf bin obj"
fi
JS='{"name":"t","version":"1.0.0","dependencies":{"eslint":"8.57.0","request":"2.88.2","express":"4.18.2","mocha":"10.4.0"}}'
if pick npm && command -v npm >/dev/null; then
  mkdir -p "$R/npm" && echo "$JS" > "$R/npm/package.json" && \
  measure "npm install" "$R/npm" "npm install --no-audit --no-fund" "rm -rf node_modules package-lock.json"
fi
if pick pnpm && command -v pnpm >/dev/null; then
  mkdir -p "$R/pnpm" && echo "$JS" > "$R/pnpm/package.json" && \
  measure "pnpm install" "$R/pnpm" "pnpm install" "rm -rf node_modules pnpm-lock.yaml"
fi
if pick cargo && command -v cargo >/dev/null; then
  cargo new --quiet --bin "$R/rs" >/dev/null 2>&1 && \
  printf 'regex = "1"\nserde_json = "1"\nchrono = "0.4"\n' >> "$R/rs/Cargo.toml" && \
  measure "cargo build (3 dependencies)" "$R/rs" "CARGO_TARGET_DIR=$R/rs-target cargo build" "rm -rf $R/rs-target"
fi
if pick docker && command -v docker >/dev/null && docker info >/dev/null 2>&1; then
  mkdir -p "$R/dk" && printf 'FROM alpine:3.20\nRUN apk add --no-cache curl git openssl bash\nRUN for i in $(seq 1 60); do echo "step $i"; done\nRUN apk add --no-cache python3 py3-pip nodejs npm\n' > "$R/dk/Dockerfile" && \
  measure "docker build (--no-cache)" "$R/dk" "docker build --no-cache -t shlim-bench ." "docker rmi shlim-bench"
  docker rmi shlim-bench >/dev/null 2>&1
fi

echo
printf '%-34s %9s %9s %7s  %s\n' "scenario" "plain B" "shlim B" "saved" "replies"
printf '%s\n' "${ROWS[@]:-none ran}"
echo "(bytes; tokens ≈ bytes/4; shlim = every reply the agent gets until the command ends)"
