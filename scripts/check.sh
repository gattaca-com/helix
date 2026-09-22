#!/usr/bin/env bash
set -uo pipefail

toolchain=nightly-2026-07-26
cd "$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

max_lines=${CHECK_MAX_LINES:-40}
pkg=${1:-}

if [[ -n $pkg ]]; then
  fmt_scope=(-p "$pkg")
  cargo_scope=(-p "$pkg")
else
  fmt_scope=(--all)
  cargo_scope=(--workspace)
fi

failed=()

denoise() {
  grep -Ev '^ *(Compiling|Checking|Finished|Blocking|Updating|Downloading|Downloaded|Fresh|Locking|Installing|Ignoring|Running|Executable|Doc-tests) ' \
  | grep -Ev '^(warning|error): `[^`]+` \([^)]*\) generated [0-9]+ (warning|error)' \
  | grep -Ev '^(error|warning): (could not compile|build failed, waiting)' \
  | grep -Ev '^ *$'
}

flatten_diagnostics() {
  awk '
    function cont(l) {
      return l ~ /^ *([0-9]+ )? *\|/ || l ~ /^ *= / || l ~ /^ *\.\.\.$/ ||
             l ~ /^ *\^+/ || l ~ /^ *(help|note): /
    }
    function flush() { if (pend) { print kind ": " msg; pend = 0 } }
    (pend || inblock) && cont($0) { next }
    match($0, /^(warning|error)(\[[^]]*\])?: /) {
      flush()
      inblock = 0
      kind = substr($0, 1, RLENGTH - 2)
      msg = substr($0, RLENGTH + 1)
      pend = 1
      next
    }
    pend && match($0, /^ *--> /) {
      print substr($0, RLENGTH + 1) ": " kind ": " msg
      pend = 0
      inblock = 1
      next
    }
    { flush(); inblock = 0; print }
    END { flush() }
  '
}

drop_passing_tests() {
  grep -Ev '^test .* \.\.\. (ok|ignored)( .*)?$' \
  | grep -Ev '^running [0-9]+ tests?$' \
  | grep -Ev '^test result: ok\.'
}

report() {
  local name=$1 body=$2 n
  n=$(printf '%s\n' "$body" | grep -c '' || true)
  printf '\n--- %s (%s lines) ---\n' "$name" "$n"
  printf '%s\n' "$body" | head -n "$max_lines"
  if (( n > max_lines )); then
    printf '... %s more lines, raise CHECK_MAX_LINES to see them\n' "$(( n - max_lines ))"
  fi
}

capture() {
  local -n _body=$1 _rc=$2
  shift 2
  local raw
  raw=$("$@" 2>&1)
  _rc=$?
  _body=$(printf '%s\n' "$raw" | flatten_diagnostics | denoise)
}

body=; rc=0
capture body rc cargo "+$toolchain" fmt "${fmt_scope[@]}" --check --message-format short
if (( rc != 0 )); then
  failed+=(fmt)
  report fmt "${body:-exit $rc}"
fi

capture body rc cargo "+$toolchain" clippy "${cargo_scope[@]}" \
  --all-features --no-deps --message-format short -- -D warnings
clippy_body=$body
if (( rc != 0 )); then
  failed+=(clippy)
  report clippy "${body:-exit $rc}"
fi

if printf '%s\n' "$clippy_body" | grep -q 'error\[E[0-9]'; then
  failed+=(test)
  printf '\n--- test ---\nskipped: the code does not compile\n'
else
  raw=$(cargo test "${cargo_scope[@]}" --all-features 2>&1)
  run_rc=$?
  run_body=$(printf '%s\n' "$raw" | drop_passing_tests | flatten_diagnostics | denoise)
  if (( run_rc != 0 )); then
    failed+=(test)
    report test "${run_body:-exit $run_rc}"
  else
    warnings=$(printf '%s\n' "$run_body" | grep -E ': warning: ' || true)
    if [[ -n $warnings ]]; then
      report "test warnings" "$warnings"
    fi
  fi
fi

if (( ${#failed[@]} == 0 )); then
  echo "check: ok"
  exit 0
fi

printf '\ncheck: FAILED [%s]\n' "${failed[*]}"
exit 1
