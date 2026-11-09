#!/bin/sh
# e2e «сертификат молчания» — тонкая POSIX-обёртка (волна ×6, ПРЕДЛОЖЕНИЕ-43 #2).
#
# Вся логика живёт в tools/silence_e2e.py (конвенция репо: std-only python,
# см. tests/architecture.py, MANIFEST.sha256 WAVE6_P43). Этот скрипт —
# «входная дверь» с каноническим именем для внешних вызовов:
#
#   scripts/silence_e2e.sh --certificate ./target/debug/neutral-browser --require-netns
#   scripts/silence_e2e.sh --selftest
#
# Семантика неизменна: ноль внеполосных запросов = SILENCE_E2E=PASS;
# любая попытка наружу = FAIL (fail-closed); без детектора = NOT_ISSUED.
# В CI вызов с флагом --certificate прописан в .github/workflows/ci.yml.
set -eu
cd "$(dirname "$0")/.."
exec python3 tools/silence_e2e.py "$@"
