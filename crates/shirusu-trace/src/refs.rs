//! イベントが参照する Blob の列挙 (一か所)。Blob はイベントの Payload (`PayloadRef::Blob`) からも、
//! 型付きの引数 (`TypedValue::BlobRef`) からも参照される。参照を探す処理が保存場所ごとに
//! 分かれていると、一部の場所だけを見る処理が生まれる: 7c5a553 のレビュー P1-1 では repair と
//! 完全性の集計が Payload だけを見て、引数から参照される Blob を修復後のファイルから落とした。
//!
//! Reader・Writer・repair・完全性の集計・conformance・Probe の切り出しは、どれもこの
//! [`TraceEvent::blob_refs`] から参照を受け取る (shirusu-lint `blob-ref-walk` が、ここの外で
//! `if let PayloadRef::Blob(..)` / `if let TypedValue::BlobRef(..)` と一種類だけ拾う書き方を拒む)。
//! 新しい格納場所を足すときは [`BlobSite`] に変種を足すと、場所ごとの扱いを書くまで
//! 利用側の網羅的な match がコンパイルを通らない。

use crate::blob::BlobId;
use crate::compact::decode_values;
use crate::error::CompactError;
use crate::model::{PayloadRef, TraceEvent};
use crate::schema::{SchemaRegistry, TypedValue};

/// Blob を参照している場所
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlobSite {
    /// イベントの Payload
    Payload,
    /// 引数値列の `index` 番目のフィールド (BlobRef 型)
    Arg { index: usize },
}

/// 一つの参照: どの Blob を、どこから
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlobUse {
    pub id: BlobId,
    pub site: BlobSite,
}

/// 引数値列を Schema で読めなかった理由 (参照を列挙しきれない)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsUnreadable {
    /// 引数値列があるのに、その Schema の型がファイルにない
    UnknownSchema { schema_id: u32 },
    /// Schema の型で復号できない
    Undecodable(CompactError),
}

/// イベントが参照する Blob のすべて、または列挙しきれないこと。
/// 読めない引数を「参照なし」として扱わない (参照を落とす側へ倒れないように)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventBlobRefs {
    /// すべての参照 (Payload が先、引数はフィールド順。同じ Blob が二度現れることもある)
    Known(Vec<BlobUse>),
    /// 引数値列が読めない。`known` は読めた側 (Payload) の参照だけ
    Partial {
        known: Vec<BlobUse>,
        why: ArgsUnreadable,
    },
}

impl EventBlobRefs {
    /// 分かっている参照 (Partial でも Payload 側は分かる)
    pub fn known(&self) -> &[BlobUse] {
        match self {
            EventBlobRefs::Known(k) | EventBlobRefs::Partial { known: k, .. } => k,
        }
    }

    /// 参照先がすべて `has` にあるか。列挙しきれない (Partial) なら None
    pub fn all_present(&self, has: impl Fn(&BlobId) -> bool) -> Option<bool> {
        let present = self.known().iter().all(|u| has(&u.id));
        match self {
            EventBlobRefs::Known(_) => Some(present),
            EventBlobRefs::Partial { .. } => (!present).then_some(false),
        }
    }

    /// `has` にない参照 (重複は除かない)
    pub fn missing(&self, has: impl Fn(&BlobId) -> bool) -> impl Iterator<Item = &BlobUse> {
        self.known().iter().filter(move |u| !has(&u.id))
    }
}

impl TraceEvent {
    /// このイベントが参照する Blob。引数値列は `registry` の Schema で読む
    pub fn blob_refs(&self, registry: &SchemaRegistry) -> EventBlobRefs {
        let mut known = Vec::new();
        match &self.payload {
            PayloadRef::Blob(id) => known.push(BlobUse {
                id: *id,
                site: BlobSite::Payload,
            }),
            PayloadRef::None | PayloadRef::Inline(_) | PayloadRef::Dropped { .. } => {}
        }
        let Some(args) = self.args.as_deref() else {
            return EventBlobRefs::Known(known);
        };
        let Some(types) = registry.types_ref(self.schema_id) else {
            return EventBlobRefs::Partial {
                known,
                why: ArgsUnreadable::UnknownSchema {
                    schema_id: self.schema_id,
                },
            };
        };
        match decode_values(types, args) {
            Ok(values) => {
                for (index, v) in values.iter().enumerate() {
                    match &v.value {
                        TypedValue::BlobRef(id) => known.push(BlobUse {
                            id: *id,
                            site: BlobSite::Arg { index },
                        }),
                        TypedValue::I32(_)
                        | TypedValue::I64(_)
                        | TypedValue::U32(_)
                        | TypedValue::U64(_)
                        | TypedValue::F32Bits(_)
                        | TypedValue::F64Bits(_)
                        | TypedValue::Bool(_)
                        | TypedValue::Str(_)
                        | TypedValue::Str16(_)
                        | TypedValue::Bytes(_)
                        | TypedValue::Ptr32(_)
                        | TypedValue::Ptr64(_) => {}
                    }
                }
                EventBlobRefs::Known(known)
            }
            Err(e) => EventBlobRefs::Partial {
                known,
                why: ArgsUnreadable::Undecodable(e),
            },
        }
    }
}
