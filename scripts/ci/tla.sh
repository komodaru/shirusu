#!/usr/bin/env bash
# TLA+ / TLC モデル検査 (CI ubuntu + Java)。tla2tools.jar を取得して実行する。
# 主モデルは全性質が成立すること、変種は期待した反例を検出すること (モデルが
# その競合を表現できる証跡) の両方を確認する。
set -euo pipefail
cd "$(dirname "$0")/../../tla"
TLA_TOOLS_SHA256="7e53aa636dac8a3cb0533f343115c0548e03fc0ce5edc7739becf58d3d0fb84f"
# A cached or downloaded jar runs as CI code: verify it every time. The
# v1.8.0 asset is a rolling pre-release, so its hash changes when upstream
# re-uploads it (last re-pinned 2026-11-15).
if [ -f tla2tools.jar ] && [ "$(sha256sum tla2tools.jar | cut -d' ' -f1)" != "$TLA_TOOLS_SHA256" ]; then
  echo "tla2tools.jar sha256 mismatch, re-downloading" >&2
  rm -f tla2tools.jar
fi
if [ ! -f tla2tools.jar ]; then
  curl -fsSL -o tla2tools.jar.tmp https://github.com/tlaplus/tlaplus/releases/download/v1.8.0/tla2tools.jar
  if [ "$(sha256sum tla2tools.jar.tmp | cut -d' ' -f1)" != "$TLA_TOOLS_SHA256" ]; then
    echo "tla2tools.jar sha256 mismatch (expected $TLA_TOOLS_SHA256)" >&2
    rm -f tla2tools.jar.tmp
    exit 1
  fi
  mv tla2tools.jar.tmp tla2tools.jar
fi
# 転送 (仕様 v0.5、レビュー 2026-11-14 V10): Payload Ring、コピーして解放して
# から検証、DETACH なしの終了。変種はどれも反例が必須。
buggy_log="$(mktemp)"
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -lncheck final -workers auto -config ShirusuTransport.cfg ShirusuTransport
expect_transport_violation() {
  local cfg="$1" pattern="$2"
  if java -cp tla2tools.jar tlc2.TLC -lncheck final -config "$cfg" ShirusuTransport >"$buggy_log" 2>&1; then
    echo "ERROR: $cfg で違反が検出されなかった (モデルが競合を表現できていない)" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  if ! grep -Eq "$pattern" "$buggy_log"; then
    echo "ERROR: $cfg で期待した違反 ($pattern) が見つからない" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  echo "OK: $cfg で $pattern を検出した"
}
expect_transport_violation ShirusuTransportBuggy.cfg "Invariant ClaimedGenerationIsCurrent is violated"
expect_transport_violation ShirusuTransportValidateShared.cfg "Invariant AcceptedIsPublished is violated"
expect_transport_violation ShirusuTransportReclaimByPayload.cfg "Temporal property DeadSlotIsReclaimed was violated"

# Registration precedes OS submission; late cancellation cannot erase reuse.
java -cp tla2tools.jar tlc2.TLC -config OverlappedLifecycle.cfg OverlappedLifecycle
if java -cp tla2tools.jar tlc2.TLC -config OverlappedLifecycleBuggy.cfg OverlappedLifecycle >"$buggy_log" 2>&1; then
  echo "ERROR: late registration mutation survived" >&2
  exit 1
fi
if ! grep -Eq 'Invariant (NoLostCompletion|NoEraseReusedOperation) is violated' "$buggy_log"; then
  cat "$buggy_log" >&2
  exit 1
fi

# data_ready の進行性: 期限付き待機があれば、閾値未満の公開や空確認と待機の
# すれ違いがあっても全イベントが回収される。期限を外した変種では反例が必須。
java -cp tla2tools.jar tlc2.TLC -config NotifyLiveness.cfg NotifyLiveness
if java -cp tla2tools.jar tlc2.TLC -config NotifyLivenessNoTimeout.cfg NotifyLiveness >"$buggy_log" 2>&1; then
  echo "ERROR: 期限なし待機の変種で進行性の違反が検出されなかった" >&2
  exit 1
fi
if ! grep -q "Temporal property Drained was violated" "$buggy_log"; then
  cat "$buggy_log" >&2
  exit 1
fi
echo "OK: 期限なし待機の変種で Drained の違反を検出した"

# 共有 vtable フックの Module Epoch: 遅れたアンロード通知が再利用後の予約を
# 退役させない。CAS を通常 store に変えた変種では反例が必須。
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -config SharedComEpoch.cfg SharedComEpoch
if java -cp tla2tools.jar tlc2.TLC -config SharedComEpochBlindStore.cfg SharedComEpoch >"$buggy_log" 2>&1; then
  echo "ERROR: CAS を外した変種で違反が検出されなかった" >&2
  exit 1
fi
if ! grep -q "Invariant NoStaleRetire is violated" "$buggy_log"; then
  cat "$buggy_log" >&2
  exit 1
fi
echo "OK: CAS を外した変種で NoStaleRetire の違反を検出した"

# 非対称同期 (WriterCell / Hazard Slot、レビュー P0/P1): Store→Load の並べ替えと
# 強制終了を含めて、排他・寿命・進行性が成り立つ。重い側のフェンスを外した変種、
# タイムアウトで奪う変種、終了状態を見ない変種 (旧実装) では反例が必須。
# -lncheck final: TLC's periodic liveness check on a partly explored graph
# reported a spurious stuttering counterexample on the 2-core CI runner (a
# state whose successors were still queued). Check liveness on the full graph.
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -lncheck final -workers auto -config AsymmetricHandshake.cfg AsymmetricHandshake
expect_violation() {
  local cfg="$1" pattern="$2"
  if java -cp tla2tools.jar tlc2.TLC -lncheck final -config "$cfg" AsymmetricHandshake >"$buggy_log" 2>&1; then
    echo "ERROR: $cfg で違反が検出されなかった" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  if ! grep -Eq "$pattern" "$buggy_log"; then
    echo "ERROR: $cfg で期待した違反 ($pattern) が見つからない" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  echo "OK: $cfg で $pattern を検出した"
}
expect_violation AsymmetricHandshakeNoFenceWriter.cfg "Invariant MutualExclusion is violated"
expect_violation AsymmetricHandshakeNoFenceHazard.cfg "Invariant HazardSafe is violated"
expect_violation AsymmetricHandshakeTimeoutWriter.cfg "Invariant NoPoisonedUse is violated"
expect_violation AsymmetricHandshakeTimeoutHazard.cfg "Invariant HazardSafe is violated"
expect_violation AsymmetricHandshakeNoLiveness.cfg "Temporal propert(y|ies) RemoteProgress.* (was|were) violated"
expect_violation AsymmetricHandshakeNoLockLiveness.cfg "Temporal propert(y|ies) RemoteProgress.* (was|were) violated"

# Runtime / Hazard の同期契約 (レビュー P2): 解放後の参照なし、排出待ちの完了後に
# 記録なし、最終的に回収される。各安全策を外した変種では反例が必須。
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -lncheck final -workers auto -config RuntimeHazard.cfg RuntimeHazard
expect_runtime_violation() {
  local cfg="$1" pattern="$2"
  if java -cp tla2tools.jar tlc2.TLC -config "$cfg" RuntimeHazard >"$buggy_log" 2>&1; then
    echo "ERROR: $cfg で違反が検出されなかった" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  if ! grep -Eq "$pattern" "$buggy_log"; then
    echo "ERROR: $cfg で期待した違反 ($pattern) が見つからない" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  echo "OK: $cfg で $pattern を検出した"
}
expect_runtime_violation RuntimeHazardNoRevisionRecheck.cfg "Invariant HazardSafe is violated"
expect_runtime_violation RuntimeHazardLightReclaim.cfg "Invariant HazardSafe is violated"
expect_runtime_violation RuntimeHazardLightWait.cfg "Invariant DrainedQuiet is violated"
expect_runtime_violation RuntimeHazardNoGateRecheck.cfg "Invariant DrainedQuiet is violated"

# Trace の保存と復旧 (writer.rs / trace_pipeline.rs / reader.rs): 失敗・部分書き込み・
# 容量不足・クラッシュ後の読み直しで、二重記録なし、保存済みは残る、Finalization を
# 読めたら全イベントが読めるか欠落として数えられている、参照する Blob がある、Index が
# 位置どおり。切り詰めない変種、未書き出し Blob を捨てる変種、「Finalization を読めたら
# finish は成功した」は反例が必須。到達確認の 2 つも違反が必須。
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -config TraceStore.cfg TraceStore
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -config TraceStoreBuffered.cfg TraceStore
java -XX:+UseParallelGC -cp tla2tools.jar tlc2.TLC -workers auto -config TraceStoreRetry.cfg TraceStore
expect_store_violation() {
  local cfg="$1" pattern="$2"
  if java -cp tla2tools.jar tlc2.TLC -config "$cfg" TraceStore >"$buggy_log" 2>&1; then
    echo "ERROR: $cfg で違反が検出されなかった" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  if ! grep -Eq "$pattern" "$buggy_log"; then
    echo "ERROR: $cfg で期待した違反 ($pattern) が見つからない" >&2
    cat "$buggy_log" >&2
    exit 1
  fi
  echo "OK: $cfg で $pattern を検出した"
}
expect_store_violation TraceStoreNoTruncate.cfg "Invariant IndexAgrees is violated"
expect_store_violation TraceStoreDropPending.cfg "Invariant RefsResolve is violated"
expect_store_violation TraceStoreDoneOnly.cfg "Invariant DoneOnly is violated"
expect_store_violation TraceStoreReachDegraded.cfg "Invariant NoDegradedFinish is violated"
expect_store_violation TraceStoreReachTorn.cfg "Invariant NeverTorn is violated"
