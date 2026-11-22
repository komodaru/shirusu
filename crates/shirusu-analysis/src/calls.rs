//! 呼び出し単位 (enter と leave の組) でしか分からない値の検査。完全性の集計
//! ([`crate::IntegrityCounts`]) が全体読み込みとストリーミング読みの両方でこれを通し、conformance の
//! `last-error-inputs` も同じ結果を報告する (d4fc3c0 レビュー 1.6: 入力で決まる LastError の照合が
//! conformance にしかなく、取得元が誤った Stale を書いた記録を、完全性・`no-integrity-loss`・比較は
//! 完全な観測として受理した)。
use std::collections::HashMap;

use shirusu_trace::compact::values_ref;
use shirusu_trace::model::{EventBatch, TraceEvent};
use shirusu_trace::schema::{EnterInputs, LastErrorMeaning, SchemaRegistry, ValueCapture};

use crate::view::{api_name_of_schema, Phase};

/// 呼び出しの鍵: (session, process, thread, call_id)
type CallKey = (u64, u64, u64, u64);

/// 入力で決まる LastError の照合 ([`LastErrorMeaning::check_stale_inputs`])
#[derive(Debug, Default, Clone)]
pub struct CallChecks {
    /// 規則が読む入力 ([`LastErrorMeaning::input_fields`]) だけを、leave まで覚える
    open: HashMap<CallKey, Vec<(&'static str, u64)>>,
    /// 照合した Stale の数
    pub checked: u64,
    /// 入力と合わない呼び出し (説明)
    pub problems: Vec<String>,
}

/// 値列の一つのフィールド (整数として読めた値)
struct Scalar<'a> {
    name: &'a str,
    capture: ValueCapture,
    flags: u32,
    bits: Option<u64>,
}

impl Scalar<'_> {
    /// 事実として使える値 (Captured だけ)
    fn fact(&self) -> Option<u64> {
        self.capture.fact(self.bits)
    }
}

/// Schema 名から関数名 (`kernel32.dll!CreateFileW` → `CreateFileW`)
fn function_of(name: &str) -> &str {
    let api = api_name_of_schema(name);
    api.rsplit('!').next().unwrap_or(api)
}

impl CallChecks {
    /// ファイル順に 1 件ずつ
    pub fn note(&mut self, registry: &SchemaRegistry, batch: &EventBatch, e: &TraceEvent) {
        let (Some(def), Some(args)) = (registry.get(e.schema_id), e.args.as_deref()) else {
            return;
        };
        let Some(shapes) = registry.shape_ref(e.schema_id) else {
            return;
        };
        let key = (batch.session_id, batch.process_id, batch.thread_id, e.call_id);
        let function = function_of(&def.name);
        // Captured の整数値だけを入力・結果として読む (値列が読めなければ何も覚えない)
        let scalars = || -> Option<Vec<Scalar<'_>>> {
            values_ref(shapes.types().iter().copied(), args)
                .zip(&def.fields)
                .map(|(v, f)| {
                    v.ok().map(|v| Scalar {
                        name: f.name.as_str(),
                        capture: v.capture,
                        flags: f.flags,
                        bits: v.value.scalar_bits(),
                    })
                })
                .collect()
        };
        match Phase::of_kind(e.kind) {
            Phase::Enter => {
                let wanted = LastErrorMeaning::input_fields(function);
                if wanted.is_empty() {
                    return;
                }
                let Some(values) = scalars() else {
                    return;
                };
                let inputs = wanted
                    .iter()
                    .filter_map(|w| {
                        values.iter().find(|v| v.name == *w).and_then(|v| v.fact().map(|b| (*w, b)))
                    })
                    .collect();
                self.open.insert(key, inputs);
            }
            Phase::Leave => {
                let enter = self.open.remove(&key);
                let Some(values) = scalars() else {
                    return;
                };
                let Some(last_error) = values.iter().find(|v| {
                    v.name == "last_error"
                        && v.capture == ValueCapture::Stale
                        && LastErrorMeaning::from_flags(v.flags) == LastErrorMeaning::DependsOnInputs
                }) else {
                    return;
                };
                self.checked += 1;
                let ret = values.iter().find(|v| v.name == "return").and_then(Scalar::fact);
                // Stale の本体は読み取った生値
                let raw = last_error.bits.and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
                let input = enter.as_ref().map(|inputs| {
                    move |name: &str| inputs.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
                });
                let at = format!("{} call {} on thread {}", api_name_of_schema(&def.name), e.call_id, batch.thread_id);
                let verdict = LastErrorMeaning::check_stale_inputs(
                    function,
                    input.as_ref().map(|f| f as EnterInputs<'_>),
                    ret,
                    raw,
                );
                self.problems.extend(verdict.problem(&at));
            }
            Phase::Aborted | Phase::Completed | Phase::Other(_) => {}
        }
    }
}
