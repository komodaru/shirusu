//! Schema と型付き値のモデル。形式の正は `docs/trace-format.md` の
//! 「Compact Binary」節。
//!
//! Schema は「ファイル内連番の Schema ID」「名前」「フィールド列」で
//! 定義され、正規化内容の BLAKE3 ハッシュ（先頭 8 バイト）で別 Trace
//! 間の同一性を判定する。一度発行した Schema ID のレイアウトは変更
//! しない。

use crate::blob::BlobId;
use crate::proto::{FieldDef, SchemaDef};

/// フィールド型タグ（FieldDef.ty の値）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Ty {
    I32,
    I64,
    U32,
    U64,
    F32,
    F64,
    Bool,
    Str,
    Str16,
    Bytes,
    BlobRef,
    Ptr32,
    Ptr64,
}

impl Ty {
    pub fn tag(self) -> u32 {
        match self {
            Self::I32 => 1,
            Self::I64 => 2,
            Self::U32 => 3,
            Self::U64 => 4,
            Self::F32 => 5,
            Self::F64 => 6,
            Self::Bool => 7,
            Self::Str => 8,
            Self::Str16 => 9,
            Self::Bytes => 10,
            Self::BlobRef => 11,
            Self::Ptr32 => 12,
            Self::Ptr64 => 13,
        }
    }
    pub fn from_tag(tag: u32) -> Option<Self> {
        Some(match tag {
            1 => Self::I32,
            2 => Self::I64,
            3 => Self::U32,
            4 => Self::U64,
            5 => Self::F32,
            6 => Self::F64,
            7 => Self::Bool,
            8 => Self::Str,
            9 => Self::Str16,
            10 => Self::Bytes,
            11 => Self::BlobRef,
            12 => Self::Ptr32,
            13 => Self::Ptr64,
            _ => return None,
        })
    }
}

/// フィールド値の意味種別（FieldDef.flags の bit2..=4 に格納する）。///
/// 型 (Ty) だけでは「64bit 整数が個数なのか、ハンドルなのか、
/// アドレスなのか」は決まらない。正規化や比較が型だけで推定すると
/// スカラーの変化を見落とすため (レビュー #4 の #7)、Schema が
/// 意味を宣言し、利用側はそれに従う。bit0/bit1 は形式仕様で
/// inout / 省略可能に予約済みのため、種別は bit2 から置く。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueKind {
    /// 未指定（種別ビットが 0）。意味情報を持たない旧来の Schema 用。
    /// 利用側は型からの推定にフォールバックする（推定の限界は
    /// 利用側の文書に明記される）。
    Unspecified,
    /// 純粋なスカラー（個数・時刻・カウンタ・真理値など）。値を
    /// そのまま比較する。U64 でもポインタとして扱ってはいけない。
    Scalar,
    /// メモリアドレス。中身は環境依存のため比較せず、存在と
    /// Null/Invalid の区別だけを見る。
    Pointer,
    /// オブジェクトへのハンドル。生成イベントとの相関で同一性を
    /// 解決し、相関できなければポインタ級として扱う。
    Handle,
    /// このフィールドの値が新しいオブジェクトの生成である
    /// （生成 API の戻り値など）。相関の起点になる。
    CreatedObject,
}

/// ValueKind を格納するフラグビット範囲 (bit2..=4)。
pub const VALUE_KIND_SHIFT: u32 = 2;
pub const VALUE_KIND_MASK: u32 = 0b111 << VALUE_KIND_SHIFT;

impl ValueKind {
    pub fn from_flags(flags: u32) -> Self {
        match (flags & VALUE_KIND_MASK) >> VALUE_KIND_SHIFT {
            1 => Self::Scalar,
            2 => Self::Pointer,
            3 => Self::Handle,
            4 => Self::CreatedObject,
            _ => Self::Unspecified,
        }
    }
    /// flags へ埋め込む値（他のビットは含まない）。
    pub fn flags(self) -> u32 {
        let v = match self {
            Self::Unspecified => 0,
            Self::Scalar => 1,
            Self::Pointer => 2,
            Self::Handle => 3,
            Self::CreatedObject => 4,
        };
        v << VALUE_KIND_SHIFT
    }
}

/// メモリの取得タイミング（FieldDef.flags の bit5..=7 に格納する）。
///
/// overview「型付きの値」節の決定: メモリの取得タイミングは Schema で
/// 引数ごとに定める。入力バッファは呼び出し前 (Before)、出力バッファは
/// 元の API が書き込んだ後 (After)、非同期 I/O は完了時 (Completion) に
/// 取得する。入出力が同じバッファなら前後の両方 (Both) を保存できる
/// ようにする。enter / leave の Schema が分かれているため、通常は
/// enter 側の全フィールドが Before、leave 側の全フィールドが After に
/// なる。Both のフィールドは enter と leave の両 Schema に現れ、それ
/// ぞれの時点で別々のスナップショットとして保存する。
/// Completion は「完了時に取得する」という宣言であり、完了の捕捉機構
/// 自体は別途必要になる（現行フックは OVERLAPPED 指定の leave 時点で
/// 未完了のため Unsupported として記録する。完了時取得は F27 の対象）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CaptureTiming {
    /// 未指定（タイミングビットが 0）。取得タイミングを宣言しない
    /// 旧来の Schema 用。利用側は enter / leave の Schema どちらに
    /// 属するかで読み替える。
    Unspecified,
    /// 呼び出し前（enter）。入力バッファの内容はここで取る。
    Before,
    /// 元の API が書き込んだ後（leave）。出力バッファの内容はここで取る。
    After,
    /// 非同期 I/O の完了時。leave 時点ではまだ完了していないため、
    /// 完了を捕捉する仕組みが値を供給するまで取得できない。
    Completion,
    /// 入出力が同じバッファで、前後両方のスナップショットを保存する。
    /// フィールドは enter と leave の両 Schema に現れる。
    Both,
}

/// CaptureTiming を格納するフラグビット範囲 (bit5..=7)。
pub const CAPTURE_TIMING_SHIFT: u32 = 5;
pub const CAPTURE_TIMING_MASK: u32 = 0b111 << CAPTURE_TIMING_SHIFT;

impl CaptureTiming {
    pub fn from_flags(flags: u32) -> Self {
        match (flags & CAPTURE_TIMING_MASK) >> CAPTURE_TIMING_SHIFT {
            1 => Self::Before,
            2 => Self::After,
            3 => Self::Completion,
            4 => Self::Both,
            _ => Self::Unspecified,
        }
    }
    /// flags へ埋め込む値（他のビットは含まない）。
    pub fn flags(self) -> u32 {
        let v = match self {
            Self::Unspecified => 0,
            Self::Before => 1,
            Self::After => 2,
            Self::Completion => 3,
            Self::Both => 4,
        };
        v << CAPTURE_TIMING_SHIFT
    }
}

/// GetLastError の生値が「API 仕様上意味のあるエラー値」である条件
/// （FieldDef.flags の bit8..=11 に格納する）。
///
/// overview「型付きの値」節の決定: `GetLastError` は生の値と、API 仕様上
/// 意味のあるエラー値を区別する。Windows の規約では、多くの API は
/// 失敗を示す戻り値の場合にだけ LastError を設定する。呼び出しが成功
/// したときに TEB に残っている生値は、以前の失敗呼び出しの残留値であり、
/// この呼び出しのエラーではない。条件は API ごとに Schema で宣言し、
/// 取得側は条件を満たさない生値を `ValueCapture::Stale` として記録する
/// （値本体は生値を保持し、再検証に使える）。`is_meaningful` は
/// leave イベントの戻り値（生値）で判定する。
/// 未指定 (Unspecified) の旧来 Schema では従来どおり生値をそのまま
/// 意味のある値として扱う（フォールバック）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LastErrorMeaning {
    /// 未指定（条件ビットが 0）。条件を宣言しない旧来の Schema 用。
    /// 利用側は生値を意味のある値として扱う。
    Unspecified,
    /// 常に意味を持つ（`GetLastError` 自体など、戻り値がエラー値
    /// そのものである API 用）。
    Always,
    /// 戻り値が FALSE / 0 の場合にだけ意味を持つ（BOOL 戻り値の API。
    /// ReadFile / WriteFile / CloseHandle）。
    WhenReturnIsFalse,
    /// 戻り値が INVALID_HANDLE_VALUE の場合にだけ意味を持つ
    /// （HANDLE 戻り値の API。旧 CreateFileW 宣言など）。
    WhenReturnIsInvalidHandle,
    /// 戻り値が NULL (0) の場合にだけ意味を持つ（HANDLE / ポインタ
    /// 戻り値の API。L03 の生成 API 用。CreateMutexW / VirtualAlloc 等）。
    WhenReturnIsNull,
    /// GetLastError に意味がない（戻り値がエラーコードそのものである
    /// API (Reg* 等) や、失敗しない・失敗を戻り値で表さない API。
    /// L03 の生成 API 用）。取得側は生値を常に Stale として記録する。
    Never,
    /// 戻り値が -1 (全ビット 1) の場合にだけ意味を持つ（WAIT_FAILED /
    /// SOCKET_ERROR 等の -1 返し。L03 の生成 API 用）。
    /// 32bit 形 (下位 32 ビットのみ 1) も失敗とみなす
    /// （WhenReturnIsInvalidHandle と同じ幅の扱い）。
    WhenReturnIsMinusOne,
    /// Non-NULL is failure (GlobalFree / LocalFree return NULL on success).
    WhenReturnIsNotNull,
    /// Meaning also depends on input arguments. Capture must evaluate the
    /// contract and mark inapplicable values Stale; a return-only query cannot
    /// prove staleness and conservatively preserves the value.
    DependsOnInputs,
    /// NULL か INVALID_HANDLE_VALUE のどちらかが失敗（どちらで失敗するか
    /// 宣言の無い HANDLE 返し。広域層の既定。2026-11-14 レビュー P3-16:
    /// NULL だけを失敗とすると、INVALID_HANDLE_VALUE で失敗する API の
    /// 本物のエラー値が Stale と記録された）。
    WhenReturnIsNullOrInvalidHandle,
}

/// LastErrorMeaning を格納するフラグビット範囲 (bit8..=11)。
pub const LAST_ERROR_MEANING_SHIFT: u32 = 8;
pub const LAST_ERROR_MEANING_MASK: u32 = 0b1111 << LAST_ERROR_MEANING_SHIFT;

impl LastErrorMeaning {
    pub fn from_flags(flags: u32) -> Self {
        match (flags & LAST_ERROR_MEANING_MASK) >> LAST_ERROR_MEANING_SHIFT {
            1 => Self::Always,
            2 => Self::WhenReturnIsFalse,
            3 => Self::WhenReturnIsInvalidHandle,
            4 => Self::WhenReturnIsNull,
            5 => Self::Never,
            6 => Self::WhenReturnIsMinusOne,
            7 => Self::WhenReturnIsNotNull,
            8 => Self::DependsOnInputs,
            9 => Self::WhenReturnIsNullOrInvalidHandle,
            _ => Self::Unspecified,
        }
    }
    /// flags へ埋め込む値（他のビットは含まない）。
    pub fn flags(self) -> u32 {
        let v = match self {
            Self::Unspecified => 0,
            Self::Always => 1,
            Self::WhenReturnIsFalse => 2,
            Self::WhenReturnIsInvalidHandle => 3,
            Self::WhenReturnIsNull => 4,
            Self::Never => 5,
            Self::WhenReturnIsMinusOne => 6,
            Self::WhenReturnIsNotNull => 7,
            Self::DependsOnInputs => 8,
            Self::WhenReturnIsNullOrInvalidHandle => 9,
        };
        v << LAST_ERROR_MEANING_SHIFT
    }

    /// DependsOnInputs の条件を、同じ呼び出しの enter の値で解く。取得側 (shirusu-hook) と
    /// 検査側 (conformance の `last-error-inputs`) が同じ規則を使うための一か所。
    /// `function` は `CreateFileW` のような関数名、`input` は enter のフィールド名で値を返す。
    /// 規則の分からない関数と、決め手の入力が無い呼び出しは DependsOnInputs のまま返す
    /// (取得側はそれを Unsupported と記録する)。
    pub fn resolve_inputs(
        function: &str,
        input: &dyn Fn(&str) -> Option<u64>,
        returned: u64,
        raw_error: u32,
    ) -> LastErrorMeaning {
        // CREATE_ALWAYS (2) / OPEN_ALWAYS (4) は成功時も ERROR_ALREADY_EXISTS を設定する
        if matches!(function, "CreateFileA" | "CreateFileW") {
            // DWORD の引数: 32 ビットに収まらない値は決め手にならない
            return match input("dwCreationDisposition").and_then(|v| u32::try_from(v).ok()) {
                Some(2 | 4) => LastErrorMeaning::Always,
                Some(_) => LastErrorMeaning::WhenReturnIsInvalidHandle,
                None => LastErrorMeaning::DependsOnInputs,
            };
        }
        // 名前付きオブジェクトの作成は、既存のものを開いた成功で ERROR_ALREADY_EXISTS
        // (183) を設定する。名前の無い作成の成功では LastError は残留値
        if matches!(
            function,
            "CreateMutexW" | "CreateMutexExW" | "CreateEventW" | "CreateEventExW" | "CreateSemaphoreW"
        ) {
            if returned == 0 {
                return LastErrorMeaning::Always;
            }
            return match input("lpName") {
                Some(0) => LastErrorMeaning::Never,
                Some(_) if raw_error == ERROR_ALREADY_EXISTS => LastErrorMeaning::Always,
                Some(_) | None => LastErrorMeaning::DependsOnInputs,
            };
        }
        LastErrorMeaning::DependsOnInputs
    }

    /// [`LastErrorMeaning::resolve_inputs`] が `function` について読む enter のフィールド名。
    /// 呼び出し単位の検査は、enter からこれだけを覚えておけばよい (規則にない関数は空)
    pub fn input_fields(function: &str) -> &'static [&'static str] {
        match function {
            "CreateFileA" | "CreateFileW" => &["dwCreationDisposition"],
            "CreateMutexW" | "CreateMutexExW" | "CreateEventW" | "CreateEventExW" | "CreateSemaphoreW" => {
                &["lpName"]
            }
            _ => &[],
        }
    }

    /// 入力で決まる LastError (DependsOnInputs) を Stale と書いた leave が、同じ呼び出しの
    /// enter の入力と合うか。取得側と同じ規則 ([`LastErrorMeaning::resolve_inputs`]) で照らす。
    /// conformance・完全性の集計・比較・テストが同じ答えを使う (d4fc3c0 レビュー 1.6: この照合が
    /// conformance にしかなく、取得元が誤った Stale を書いた記録を、完全性の判定は完全とした)。
    /// `enter` は enter が見つからなければ None、`ret` は Captured の戻り値。
    pub fn check_stale_inputs(
        function: &str,
        enter: Option<EnterInputs<'_>>,
        ret: Option<u64>,
        raw_error: u32,
    ) -> StaleInputs {
        let Some(input) = enter else {
            return StaleInputs::NoEnter;
        };
        let Some(ret) = ret else {
            return StaleInputs::NoReturn;
        };
        let meaning = LastErrorMeaning::resolve_inputs(function, input, ret, raw_error);
        if meaning.is_meaningful(ret) {
            StaleInputs::Meaningful { meaning, ret }
        } else {
            StaleInputs::Agrees
        }
    }

    /// LastError の意味が何で決まるか。条件の種類ごとに必要な材料が違う: Never / Always は
    /// 結果を見なくても決まり、戻り値を必要としない (d4fc3c0 レビュー P1: Never を戻り値で決まる
    /// 条件と同じ腕に置き、戻り値のない Sleep などの正しい Stale を不完全と数えた)。
    pub fn rule(self) -> LastErrorRule {
        match self {
            Self::Unspecified | Self::Always => LastErrorRule::Fixed { meaningful: true },
            Self::Never => LastErrorRule::Fixed { meaningful: false },
            Self::WhenReturnIsFalse => LastErrorRule::OnReturn(ReturnTest::IsFalse),
            Self::WhenReturnIsInvalidHandle => LastErrorRule::OnReturn(ReturnTest::IsInvalidHandle),
            Self::WhenReturnIsNull => LastErrorRule::OnReturn(ReturnTest::IsNull),
            Self::WhenReturnIsMinusOne => LastErrorRule::OnReturn(ReturnTest::IsMinusOne),
            Self::WhenReturnIsNotNull => LastErrorRule::OnReturn(ReturnTest::IsNotNull),
            Self::WhenReturnIsNullOrInvalidHandle => {
                LastErrorRule::OnReturn(ReturnTest::IsNullOrInvalidHandle)
            }
            Self::DependsOnInputs => LastErrorRule::OnInputs,
        }
    }

    /// 戻り値の生値が分かっているとき、この API の LastError 生値が意味を持つか。
    /// `return_value` は leave Schema の `return` フィールドの生値
    /// （HANDLE は U64、BOOL は I32 の生値として u64 へ写したもの）。
    /// DependsOnInputs は戻り値だけでは決まらないので true (値を残す側) を返す:
    /// 入力まで照らすのは [`LastErrorMeaning::resolve_inputs`]。
    /// 戻り値が記録にないかもしれない場面では [`LastErrorMeaning::rule`] で分ける。
    pub fn is_meaningful(self, return_value: u64) -> bool {
        match self.rule() {
            LastErrorRule::Fixed { meaningful } => meaningful,
            LastErrorRule::OnReturn(t) => t.holds(return_value),
            LastErrorRule::OnInputs => true,
        }
    }
}

/// enter のフィールド名から、その Captured の値を引く
pub type EnterInputs<'a> = &'a dyn Fn(&str) -> Option<u64>;

/// [`LastErrorMeaning::check_stale_inputs`] の答え
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleInputs {
    /// 入力からも古い値と言える
    Agrees,
    /// 入力を照らす enter がない
    NoEnter,
    /// 戻り値が取れていない
    NoReturn,
    /// 入力によれば LastError に意味がある (Stale は誤り)
    Meaningful { meaning: LastErrorMeaning, ret: u64 },
}

impl StaleInputs {
    /// 誤りなら説明 (`at` は「どの呼び出しか」)
    pub fn problem(self, at: &str) -> Option<String> {
        match self {
            StaleInputs::Agrees => None,
            StaleInputs::NoEnter => Some(format!("{at}: Stale last_error has no enter to resolve its inputs")),
            StaleInputs::NoReturn => Some(format!("{at}: Stale last_error without a captured return")),
            StaleInputs::Meaningful { meaning, ret } => Some(format!(
                "{at}: last_error is Stale but the inputs make it {meaning:?} for return {ret:#x}"
            )),
        }
    }
}

/// [`LastErrorMeaning::rule`]: LastError の意味を決める材料
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LastErrorRule {
    /// 結果によらず決まっている (戻り値を必要としない)
    Fixed { meaningful: bool },
    /// 戻り値がこの条件を満たすときだけ意味を持つ
    OnReturn(ReturnTest),
    /// 入力でも決まる ([`LastErrorMeaning::resolve_inputs`])
    OnInputs,
}

/// 失敗を示す戻り値の条件 (戻り値の生値で判定する)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReturnTest {
    /// FALSE / 0
    IsFalse,
    /// INVALID_HANDLE_VALUE (ポインタ幅依存: 64bit は全ビット 1、32bit は下位 32 ビットのみ 1。
    /// レビュー第5回 #5)
    IsInvalidHandle,
    /// NULL (0)
    IsNull,
    /// -1 (全ビット 1。32bit 形も同じ扱い)
    IsMinusOne,
    /// 非 NULL
    IsNotNull,
    /// NULL か INVALID_HANDLE_VALUE
    IsNullOrInvalidHandle,
}

impl ReturnTest {
    pub fn holds(self, return_value: u64) -> bool {
        let all_ones = return_value == u64::MAX || return_value == 0x0000_0000_ffff_ffff;
        match self {
            Self::IsFalse | Self::IsNull => return_value == 0,
            Self::IsInvalidHandle | Self::IsMinusOne => all_ones,
            Self::IsNotNull => return_value != 0,
            Self::IsNullOrInvalidHandle => return_value == 0 || all_ones,
        }
    }
}

/// ReadFile などがメッセージの一部だけを読んだときの LastError (部分データは有効で、残りは
/// 次の呼び出しで読める: Microsoft の ReadFile の説明)
/// 名前付きオブジェクトが既にあった (成功時にも設定される)
pub const ERROR_ALREADY_EXISTS: u32 = 183;
pub const ERROR_MORE_DATA: u32 = 234;
/// OVERLAPPED 操作が開始したがまだ完了していない
pub const ERROR_IO_PENDING: u32 = 997;

/// 呼び出し後に取る出力が「どの結果なら定義されるか」（FieldDef.flags の bit12..=14）。
///
/// [`CaptureTiming`] は「いつ取るか」で、これは「どの結果なら値があるか」。NotApplicable
/// （この結果では値が定義されない）と書けるのは、この条件がそのイベントの実際の結果で
/// 偽になるときだけ（7c5a553 レビュー P2-1: 取得時期が After であることだけを根拠に、
/// 正常に戻った関数の戻り値まで NotApplicable として受理した）。条件は同じイベントの
/// `return` / `last_error` / `dequeuedOverlapped` フィールドの値で評価する ([`ResultFacts`])。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutputCondition {
    /// 未指定。NotApplicable は認めない (条件を宣言していないので)
    Unspecified,
    /// どの結果でも定義される
    Always,
    /// 戻り値が非 0 (TRUE) のときだけ
    WhenReturnIsTrue,
    /// 戻り値が非 0、または LastError が ERROR_MORE_DATA (部分データ) のとき
    WhenReturnIsTrueOrMoreData,
    /// 保留 (FALSE + ERROR_IO_PENDING) でなければ定義される
    WhenNotPending,
    /// 完了ポートからパケットを取り出したとき (戻り値が非 0、または取り出した
    /// OVERLAPPED が非 NULL: GetQueuedCompletionStatus)
    WhenPacketDequeued,
}

pub const OUTPUT_CONDITION_SHIFT: u32 = 12;
pub const OUTPUT_CONDITION_MASK: u32 = 0b111 << OUTPUT_CONDITION_SHIFT;

impl OutputCondition {
    pub fn from_flags(flags: u32) -> Self {
        match (flags & OUTPUT_CONDITION_MASK) >> OUTPUT_CONDITION_SHIFT {
            1 => Self::Always,
            2 => Self::WhenReturnIsTrue,
            3 => Self::WhenReturnIsTrueOrMoreData,
            4 => Self::WhenNotPending,
            5 => Self::WhenPacketDequeued,
            _ => Self::Unspecified,
        }
    }
    pub fn flags(self) -> u32 {
        let v = match self {
            Self::Unspecified => 0,
            Self::Always => 1,
            Self::WhenReturnIsTrue => 2,
            Self::WhenReturnIsTrueOrMoreData => 3,
            Self::WhenNotPending => 4,
            Self::WhenPacketDequeued => 5,
        };
        v << OUTPUT_CONDITION_SHIFT
    }

    /// この結果で出力が定義されるか。判定に要る値が記録にない (取れていない) なら None
    pub fn defined(self, facts: &ResultFacts) -> Option<bool> {
        match self {
            Self::Unspecified | Self::Always => Some(true),
            Self::WhenReturnIsTrue => facts.ret.map(|r| r != 0),
            Self::WhenReturnIsTrueOrMoreData => match facts.ret? {
                0 => facts.last_error.map(|e| e == ERROR_MORE_DATA),
                _ => Some(true),
            },
            Self::WhenNotPending => match facts.ret? {
                0 => facts.last_error.map(|e| e != ERROR_IO_PENDING),
                _ => Some(true),
            },
            Self::WhenPacketDequeued => match facts.ret? {
                0 => facts.dequeued.map(|o| o != 0),
                _ => Some(true),
            },
        }
    }
}

/// 結果を決めるフィールドの位置 (Schema の登録時に名前から一度だけ決める)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FieldRoles {
    ret: Option<usize>,
    last_error: Option<usize>,
    dequeued: Option<usize>,
}

/// GetQueuedCompletionStatus の leave で、取り出した OVERLAPPED の値を持つフィールドの名前
pub const DEQUEUED_OVERLAPPED_FIELD: &str = "dequeuedOverlapped";

impl FieldRoles {
    pub fn of(fields: &[crate::proto::FieldDef]) -> Self {
        let at = |name: &str| fields.iter().position(|f| f.name == name);
        FieldRoles {
            ret: at("return"),
            last_error: at("last_error"),
            dequeued: at(DEQUEUED_OVERLAPPED_FIELD),
        }
    }
}

/// 一つのイベントの実際の結果 (取得できた値だけ: Captured でない値は事実に使わない)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ResultFacts {
    /// 戻り値の生値 (BOOL / I32 は符号拡張: 取得側の `a0 as u64` と同じ)
    pub ret: Option<u64>,
    pub last_error: Option<u32>,
    pub dequeued: Option<u64>,
}

impl ResultFacts {
    /// 値列の i 番目 = (取得状態, スカラーの生値)。Captured の値だけを事実に使う
    pub fn of(roles: FieldRoles, values: impl IntoIterator<Item = (ValueCapture, Option<u64>)>) -> Self {
        match Self::try_of::<core::convert::Infallible>(roles, values.into_iter().map(Ok)) {
            Ok(facts) => facts,
            Err(never) => match never {},
        }
    }

    /// [`ResultFacts::of`] の、読みながら失敗しうる値列版 (最初の Err を返す。値列は最後まで
    /// 読む: 末尾の検査も値列の読み手に任せる)
    pub fn try_of<E>(
        roles: FieldRoles,
        values: impl Iterator<Item = Result<(ValueCapture, Option<u64>), E>>,
    ) -> Result<Self, E> {
        let mut facts = ResultFacts::default();
        for (i, v) in values.enumerate() {
            let (capture, bits) = v?;
            let bits = capture.fact(bits);
            if roles.ret == Some(i) {
                facts.ret = bits;
            }
            if roles.last_error == Some(i) {
                facts.last_error = bits.and_then(|v| u32::try_from(v).ok());
            }
            if roles.dequeued == Some(i) {
                facts.dequeued = bits;
            }
        }
        Ok(facts)
    }

    /// 復号済みの値列から
    pub fn of_values<H: Holds>(roles: FieldRoles, values: &[CapturedValue<H>]) -> Self {
        Self::of(roles, values.iter().map(|v| (v.capture, v.value.scalar_bits())))
    }
}

/// [`TypedValue`] の可変長の変種 (Str / Str16 / Bytes / BlobRef) とポインタの変種
/// (Ptr32 / Ptr64) が持つもの。[`Owned`] はトレースの値 (既定)。[`Generic`] は
/// 「生成表の型 (GenericScalar: 整数・Bool・浮動小数点) しか来ない」ことを型で言う:
/// ほかの変種の中身が never 型 `!` (Rust 1.100) なので値が作れず、match でその腕を
/// 書かなくてよい (`_ => unreachable!()` が要らない)。
pub trait Holds {
    type Str: core::fmt::Debug + Clone + PartialEq;
    type Str16: core::fmt::Debug + Clone + PartialEq;
    type Bytes: core::fmt::Debug + Clone + PartialEq;
    type Blob: core::fmt::Debug + Clone + PartialEq;
    type Ptr32: core::fmt::Debug + Clone + PartialEq;
    type Ptr64: core::fmt::Debug + Clone + PartialEq;
    /// ポインタの生値 (結果の判定では整数と同じに扱う: d4fc3c0 レビュー 2.1 で、Ptr32 / Ptr64 の
    /// 戻り値が結果情報から落ち、WhenReturnIsNull の Stale を判定できなかった)
    fn ptr32_bits(p: &Self::Ptr32) -> u64;
    fn ptr64_bits(p: &Self::Ptr64) -> u64;
}

/// トレースの値: 可変長の変種も持てる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owned {}

impl Holds for Owned {
    type Str = String;
    type Str16 = Vec<u16>;
    type Bytes = Vec<u8>;
    type Blob = BlobId;
    type Ptr32 = u32;
    type Ptr64 = u64;
    fn ptr32_bits(p: &u32) -> u64 {
        u64::from(*p)
    }
    fn ptr64_bits(p: &u64) -> u64 {
        *p
    }
}

/// 値列のバイト列を借りたまま読んだ値 ([`crate::compact::values_ref`])。文字列とバイト列を
/// 複製しないので、検証・結果の判定・Blob 参照の列挙が大きな引数でも割り当てない
/// (d4fc3c0 レビュー 4: 参照を探すために所有権付きで復号し、1 MiB の Bytes を複製した)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Borrowed<'a>(core::convert::Infallible, core::marker::PhantomData<&'a ()>);

/// UTF-16 の符号単位列 (リトルエンディアンの生バイト、長さは偶数)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Str16Bytes<'a>(&'a [u8]);

impl<'a> Str16Bytes<'a> {
    /// 長さが奇数なら None
    pub fn new(bytes: &'a [u8]) -> Option<Self> {
        bytes.len().is_multiple_of(2).then_some(Self(bytes))
    }
    pub fn units(self) -> impl Iterator<Item = u16> + 'a {
        self.0.as_chunks::<2>().0.iter().map(|b| u16::from_le_bytes(*b))
    }
    pub fn len(self) -> usize {
        self.0.len() / 2
    }
    pub fn is_empty(self) -> bool {
        self.0.is_empty()
    }
}

impl<'a> Holds for Borrowed<'a> {
    type Str = &'a str;
    type Str16 = Str16Bytes<'a>;
    type Bytes = &'a [u8];
    type Blob = BlobId;
    type Ptr32 = u32;
    type Ptr64 = u64;
    fn ptr32_bits(p: &u32) -> u64 {
        u64::from(*p)
    }
    fn ptr64_bits(p: &u64) -> u64 {
        *p
    }
}

impl TypedValue<Borrowed<'_>> {
    /// 所有権付きの値へ (復号 `decode_values` はこれで作る: 読み方は一か所)
    pub fn to_owned_value(&self) -> TypedValue {
        match self {
            TypedValue::I32(x) => TypedValue::I32(*x),
            TypedValue::I64(x) => TypedValue::I64(*x),
            TypedValue::U32(x) => TypedValue::U32(*x),
            TypedValue::U64(x) => TypedValue::U64(*x),
            TypedValue::F32Bits(x) => TypedValue::F32Bits(*x),
            TypedValue::F64Bits(x) => TypedValue::F64Bits(*x),
            TypedValue::Bool(x) => TypedValue::Bool(*x),
            TypedValue::Str(s) => TypedValue::Str((*s).to_string()),
            TypedValue::Str16(u) => TypedValue::Str16(u.units().collect()),
            TypedValue::Bytes(b) => TypedValue::Bytes(b.to_vec()),
            TypedValue::BlobRef(id) => TypedValue::BlobRef(*id),
            TypedValue::Ptr32(p) => TypedValue::Ptr32(*p),
            TypedValue::Ptr64(p) => TypedValue::Ptr64(*p),
        }
    }
}

/// 生成表の引数・戻り値 (GenericScalar の型だけ): 可変長とポインタの変種は作れない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Generic {}

impl Holds for Generic {
    type Str = !;
    type Str16 = !;
    type Bytes = !;
    type Blob = !;
    type Ptr32 = !;
    type Ptr64 = !;
    fn ptr32_bits(p: &!) -> u64 {
        *p
    }
    fn ptr64_bits(p: &!) -> u64 {
        *p
    }
}

/// 型付き値。生ビット列を保持する方針（浮動小数点はビットで保持し、
/// NaN ペイロードを失わない）に従い、F32/F64 はビット表現で持つ。
#[derive(Debug, Clone, PartialEq)]
pub enum TypedValue<H: Holds = Owned> {
    I32(i32),
    I64(i64),
    U32(u32),
    U64(u64),
    F32Bits(u32),
    F64Bits(u64),
    Bool(bool),
    Str(H::Str),
    Str16(H::Str16),
    Bytes(H::Bytes),
    BlobRef(H::Blob),
    Ptr32(H::Ptr32),
    Ptr64(H::Ptr64),
}

/// 生成表の値はトレースの値でもある (ほかの変種は `!` なので腕が要らない)。
impl From<TypedValue<Generic>> for TypedValue {
    fn from(v: TypedValue<Generic>) -> TypedValue {
        match v {
            TypedValue::I32(x) => TypedValue::I32(x),
            TypedValue::I64(x) => TypedValue::I64(x),
            TypedValue::U32(x) => TypedValue::U32(x),
            TypedValue::U64(x) => TypedValue::U64(x),
            TypedValue::F32Bits(x) => TypedValue::F32Bits(x),
            TypedValue::F64Bits(x) => TypedValue::F64Bits(x),
            TypedValue::Bool(x) => TypedValue::Bool(x),
        }
    }
}

impl<H: Holds> TypedValue<H> {
    /// 結果の判定に使う整数の生値 (I32 は符号拡張、Bool は 0/1、ポインタはアドレス)。
    /// 整数でない変種 (浮動小数点・文字列・バイト列・Blob) は None
    pub fn scalar_bits(&self) -> Option<u64> {
        match self {
            Self::I32(x) => Some(i64::from(*x) as u64),
            Self::I64(x) => Some(*x as u64),
            Self::U32(x) => Some(u64::from(*x)),
            Self::U64(x) => Some(*x),
            Self::Bool(b) => Some(u64::from(*b)),
            Self::Ptr32(p) => Some(H::ptr32_bits(p)),
            Self::Ptr64(p) => Some(H::ptr64_bits(p)),
            Self::F32Bits(_)
            | Self::F64Bits(_)
            | Self::Str(_)
            | Self::Str16(_)
            | Self::Bytes(_)
            | Self::BlobRef(_) => None,
        }
    }

    pub fn ty(&self) -> Ty {
        match self {
            Self::I32(_) => Ty::I32,
            Self::I64(_) => Ty::I64,
            Self::U32(_) => Ty::U32,
            Self::U64(_) => Ty::U64,
            Self::F32Bits(_) => Ty::F32,
            Self::F64Bits(_) => Ty::F64,
            Self::Bool(_) => Ty::Bool,
            Self::Str(_) => Ty::Str,
            Self::Str16(_) => Ty::Str16,
            Self::Bytes(_) => Ty::Bytes,
            Self::BlobRef(_) => Ty::BlobRef,
            Self::Ptr32(_) => Ty::Ptr32,
            Self::Ptr64(_) => Ty::Ptr64,
        }
    }
}

/// 値ごとの取得状態（Compact Binary の値列で使う 1 バイト）。
/// 読めなかった値を NULL として保存しないための区別であり、
/// Unreadable / Unsupported の値本体はダミー（0 / 空）が入る。
/// Stale だけは例外で、値本体は読み取った生値を保持する（再検証のため）。
/// 比較・表示では意味のある値として扱ってはならない（F20）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueCapture {
    Captured,
    Unreadable,
    Truncated,
    Unsupported,
    LimitExceeded,
    /// 値は読めたが生値が古い（この呼び出しのエラーではない）。
    /// GetLastError の生値が、戻り値が成功を示す場合に残る残留値で
    /// あることを表す（F20）。値本体は生値を保持する。
    Stale,
    /// The API defines no value for this output on the observed result.
    NotApplicable,
}

impl ValueCapture {
    /// この取得状態が、`flags` を持つフィールドについて、実際の結果 `facts` のもとで
    /// 「取得すべき値を取得できた」と言えるか。
    ///
    /// Stale と NotApplicable は「その結果では値が定義されない」ことの表明であり、
    /// 宣言だけでなく実際の結果と照合する (7c5a553 レビュー P2-1):
    /// - Stale は LastError の意味条件を宣言したフィールドで、その条件が実際の結果で
    ///   偽のときだけ (戻り値が FALSE なのに Stale と書いた LastError は取得失敗と同じ)。
    ///   Never は結果によらず偽なので戻り値を要らない (d4fc3c0 レビュー P1)。戻り値で決まる
    ///   条件は、戻り値が取れていない記録では古い値だと示せないので数えない。DependsOnInputs は
    ///   戻り値で言える必要条件 (呼び出しが成功している) だけをここで見る。
    /// - NotApplicable は呼び出し後に取る出力 (After / Completion / Both) で、
    ///   [`OutputCondition`] が実際の結果で偽のときだけ。
    pub fn is_complete_given(self, flags: u32, facts: &ResultFacts) -> bool {
        // 変種を足したら、ここで完全性に数えるかを決めるまでコンパイルが通らない
        match self {
            Self::Captured => true,
            Self::Stale => match LastErrorMeaning::from_flags(flags).rule() {
                // Never は戻り値によらず意味がない: 戻り値のない API (Sleep) でも Stale が正しい
                LastErrorRule::Fixed { meaningful } => !meaningful,
                LastErrorRule::OnReturn(test) => facts.ret.is_some_and(|r| !test.holds(r)),
                // 入力で決まる条件は、どの戻り値が失敗かも関数ごとに違う (CreateMutexW は NULL、
                // CreateFileW は INVALID_HANDLE_VALUE)。leave 単体で言えるのは「古い値だと照らす
                // 戻り値が記録にある」ことだけで、照合は enter と組にした呼び出し単位の検査
                // ([`LastErrorMeaning::check_stale_inputs`]) が行う。完全性の集計・conformance・
                // 比較はどれもその結果を使う (d4fc3c0 レビュー 1.6)。以前の「NULL でも
                // INVALID_HANDLE_VALUE でもない」は、CreateMutexW の成功を失敗と取り違えうる
                LastErrorRule::OnInputs => facts.ret.is_some(),
            },
            Self::NotApplicable => {
                let after = match CaptureTiming::from_flags(flags) {
                    CaptureTiming::Unspecified | CaptureTiming::Before => false,
                    CaptureTiming::After | CaptureTiming::Completion | CaptureTiming::Both => true,
                };
                after && OutputCondition::from_flags(flags).defined(facts) == Some(false)
            }
            Self::Unreadable | Self::Truncated | Self::Unsupported | Self::LimitExceeded => false,
        }
    }

    /// 値本体が読み取った値を持つか、形式上のダミー (0 / 空) か (trace-format「値列の
    /// レイアウト」)。完全性 ([`ValueCapture::is_complete_given`]) とは別の問い: Truncated は
    /// 不完全だが、本体は読めた部分を持つ (その Blob も参照として残す)。
    /// d4fc3c0 レビュー P2: Blob 参照の列挙がこれを見ず、NotApplicable のダミー (ゼロの Blob ID)
    /// を存在しない Blob への参照と数え、Writer の拒否と repair でのイベントの脱落に広がった。
    pub fn body(self) -> ValueBody {
        match self {
            // Stale は生値、Truncated / LimitExceeded は上限までに読めた部分を本体に持つ
            Self::Captured | Self::Truncated | Self::LimitExceeded | Self::Stale => ValueBody::Read,
            Self::Unreadable | Self::Unsupported | Self::NotApplicable => ValueBody::Dummy,
        }
    }

    /// 結果・入力の事実として使える値か: Captured の値だけ (古い値・ダミー・一部は使わない)
    pub fn fact(self, bits: Option<u64>) -> Option<u64> {
        match self {
            Self::Captured => bits,
            Self::Unreadable
            | Self::Truncated
            | Self::Unsupported
            | Self::LimitExceeded
            | Self::Stale
            | Self::NotApplicable => None,
        }
    }

    pub fn code(self) -> u8 {
        match self {
            Self::Captured => 0,
            Self::Unreadable => 1,
            Self::Truncated => 2,
            Self::Unsupported => 3,
            Self::LimitExceeded => 4,
            Self::Stale => 5,
            Self::NotApplicable => 6,
        }
    }
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::Captured,
            1 => Self::Unreadable,
            2 => Self::Truncated,
            3 => Self::Unsupported,
            4 => Self::LimitExceeded,
            5 => Self::Stale,
            6 => Self::NotApplicable,
            _ => return None,
        })
    }
}

/// [`ValueCapture::body`]: 値本体の中身
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ValueBody {
    /// 読み取った値 (一部だけのこともある)
    Read,
    /// 形式上のダミー (0 / 空): 値として読んではならない
    Dummy,
}

/// 取得状態付きの値（値列の要素）。
#[derive(Debug, Clone, PartialEq)]
pub struct CapturedValue<H: Holds = Owned> {
    pub capture: ValueCapture,
    pub value: TypedValue<H>,
}

impl From<CapturedValue<Generic>> for CapturedValue {
    fn from(v: CapturedValue<Generic>) -> CapturedValue {
        CapturedValue {
            capture: v.capture,
            value: v.value.into(),
        }
    }
}

impl<H: Holds> CapturedValue<H> {
    pub fn captured(value: TypedValue<H>) -> Self {
        Self {
            capture: ValueCapture::Captured,
            value,
        }
    }

    /// 値本体が読み取った値なら、その値。ダミー ([`ValueBody::Dummy`]) なら None
    pub fn read_value(&self) -> Option<&TypedValue<H>> {
        match self.capture.body() {
            ValueBody::Read => Some(&self.value),
            ValueBody::Dummy => None,
        }
    }
}

/// イベント単位の取得状態。転送 `CaptureState` の意味を保ち、本形式の
/// 連番で表す。未知の番号は `Other` として保持し、往復で失わない。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EventCapture {
    Captured,
    MemoryUnreadable,
    CaptureLimitExceeded,
    DroppedByBackpressure,
    Truncated,
    Unsupported,
    Other(u8),
}

impl EventCapture {
    pub fn code(self) -> u8 {
        match self {
            Self::Captured => 0,
            Self::MemoryUnreadable => 1,
            Self::CaptureLimitExceeded => 2,
            Self::DroppedByBackpressure => 3,
            Self::Truncated => 4,
            Self::Unsupported => 5,
            Self::Other(c) => c,
        }
    }
    pub fn from_code(code: u8) -> Self {
        match code {
            0 => Self::Captured,
            1 => Self::MemoryUnreadable,
            2 => Self::CaptureLimitExceeded,
            3 => Self::DroppedByBackpressure,
            4 => Self::Truncated,
            5 => Self::Unsupported,
            c => Self::Other(c),
        }
    }
}

/// Schema の正規化内容ハッシュ（BLAKE3 先頭 8 バイト、LE u64）。
/// 正規化は「名前のバイト列＋フィールドごとに (名前, ty, flags) を
/// LEB128 で並べたもの」。別 Trace 間の同一性判定の目安であり、
/// セキュリティ用途ではない（trace-format.md の決定）。
pub fn schema_content_hash(name: &str, fields: &[FieldDef]) -> u64 {
    let mut h = blake3::Hasher::new();
    h.update(name.as_bytes());
    h.update(&[0xff]); // 名前とフィールド列の境界を曖昧にしない区切り
    for f in fields {
        h.update(f.name.as_bytes());
        h.update(&[0]);
        h.update(&f.ty.to_le_bytes());
        h.update(&f.flags.to_le_bytes());
    }
    let digest = h.finalize();
    shirusu_bytes::u64_at::<0, 32>(digest.as_bytes())
}

/// 検証済み Schema 定義から Compact 用の型列を取り出す。
/// 未知の型タグが含まれていたら None（その Schema での復号は拒否）。
pub fn schema_types(def: &SchemaDef) -> Option<Vec<Ty>> {
    def.fields.iter().map(|f| Ty::from_tag(f.ty)).collect()
}

/// Schema の型列と flags 列の組。同じ長さであることは登録時に一度だけ作ることで保つ
/// (フィールドは非公開: 長さの違う 2 つの列からは作れない)。
#[derive(Debug, Clone, Copy)]
pub struct FieldShapes<'a> {
    types: &'a [Ty],
    flags: &'a [u32],
    roles: FieldRoles,
}

impl<'a> FieldShapes<'a> {
    pub fn types(self) -> &'a [Ty] {
        self.types
    }
    /// 結果を決めるフィールドの位置 (Stale / NotApplicable の照合に使う)
    pub fn roles(self) -> FieldRoles {
        self.roles
    }
    pub fn iter(self) -> impl Iterator<Item = (Ty, u32)> + 'a {
        self.types.iter().copied().zip(self.flags.iter().copied())
    }
}

/// ファイル内の Schema 集合。定義の登録時に正規化ハッシュを検証する。
#[derive(Debug, Default)]
pub struct SchemaRegistry {
    by_id: std::collections::HashMap<u32, SchemaDef>,
    order: Vec<u32>,
    types_by_id: std::collections::HashMap<u32, Vec<Ty>>,
    flags_by_id: std::collections::HashMap<u32, Vec<u32>>,
    roles_by_id: std::collections::HashMap<u32, FieldRoles>,
}

impl SchemaRegistry {
    pub fn new() -> Self {
        Self::default()
    }
    /// 定義を登録する。content_hash が正規化ハッシュと一致しない定義、
    /// schema_id 0、既存 ID と異なる内容の再定義は拒否する。
    /// 同一内容の再登録（Checkpoint での複製）は冪等に成功する。
    pub fn register(&mut self, def: SchemaDef) -> Result<(), String> {
        if def.schema_id == 0 {
            return Err("schema_id 0 is reserved".into());
        }
        let expected = schema_content_hash(&def.name, &def.fields);
        if def.content_hash != expected {
            return Err(format!(
                "schema {} content_hash mismatch: stored {:#x}, computed {:#x}",
                def.schema_id, def.content_hash, expected
            ));
        }
        if let Some(existing) = self.by_id.get(&def.schema_id) {
            if existing == &def {
                return Ok(());
            }
            return Err(format!(
                "schema_id {} redefined with different content",
                def.schema_id
            ));
        }
        if let Some(types) = schema_types(&def) {
            self.types_by_id.insert(def.schema_id, types);
            self.flags_by_id
                .insert(def.schema_id, def.fields.iter().map(|f| f.flags).collect());
            self.roles_by_id
                .insert(def.schema_id, FieldRoles::of(&def.fields));
        }
        self.order.push(def.schema_id);
        self.by_id.insert(def.schema_id, def);
        Ok(())
    }
    pub fn get(&self, schema_id: u32) -> Option<&SchemaDef> {
        self.by_id.get(&schema_id)
    }
    pub fn types(&self, schema_id: u32) -> Option<Vec<Ty>> {
        self.types_ref(schema_id).map(<[Ty]>::to_vec)
    }
    /// Borrow types computed once when the immutable schema was registered.
    pub fn types_ref(&self, schema_id: u32) -> Option<&[Ty]> {
        self.types_by_id.get(&schema_id).map(Vec::as_slice)
    }
    /// 型と同じ並びの、各フィールドの flags (型が引ける Schema だけ)。
    pub fn shape_ref(&self, schema_id: u32) -> Option<FieldShapes<'_>> {
        Some(FieldShapes {
            types: self.types_by_id.get(&schema_id)?,
            flags: self.flags_by_id.get(&schema_id)?,
            roles: self.roles_by_id.get(&schema_id).copied().unwrap_or_default(),
        })
    }
    pub fn len(&self) -> usize {
        self.by_id.len()
    }
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
    /// 登録順の Schema ID 列。
    pub fn ids(&self) -> &[u32] {
        &self.order
    }
}

/// フィールド列から SchemaDef を組み立てる（content_hash を計算して埋める）。
pub fn make_schema(schema_id: u32, name: &str, fields: Vec<(String, Ty, u32)>) -> SchemaDef {
    let fields: Vec<FieldDef> = fields
        .into_iter()
        .map(|(name, ty, flags)| FieldDef {
            name,
            ty: ty.tag(),
            flags,
        })
        .collect();
    SchemaDef {
        schema_id,
        name: name.to_string(),
        content_hash: schema_content_hash(name, &fields),
        fields,
        annotation: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_kind_flags_roundtrip() {
        for kind in [
            ValueKind::Unspecified,
            ValueKind::Scalar,
            ValueKind::Pointer,
            ValueKind::Handle,
            ValueKind::CreatedObject,
        ] {
            assert_eq!(ValueKind::from_flags(kind.flags()), kind);
            // 種別は bit0/bit1 (形式仕様の予約) に触れない
            assert_eq!(kind.flags() & 0b11, 0);
            // 他のビットと合成しても読み戻せる
            assert_eq!(ValueKind::from_flags(kind.flags() | 0b11), kind);
        }
        // 未知の種別値は Unspecified として読む (前方互換)
        assert_eq!(
            ValueKind::from_flags(7 << VALUE_KIND_SHIFT),
            ValueKind::Unspecified
        );
        assert_eq!(ValueKind::from_flags(0), ValueKind::Unspecified);
    }

    #[test]
    fn value_kind_changes_content_hash() {
        // 意味が違う Schema は別物として判定される (flags は
        // content_hash の入力に含まれる)。
        let scalar = make_schema(
            1,
            "a!B/leave",
            vec![("return".into(), Ty::U64, ValueKind::Scalar.flags())],
        );
        let created = make_schema(
            1,
            "a!B/leave",
            vec![("return".into(), Ty::U64, ValueKind::CreatedObject.flags())],
        );
        assert_ne!(scalar.content_hash, created.content_hash);
    }

    #[test]
    fn capture_timing_flags_roundtrip() {
        for timing in [
            CaptureTiming::Unspecified,
            CaptureTiming::Before,
            CaptureTiming::After,
            CaptureTiming::Completion,
            CaptureTiming::Both,
        ] {
            assert_eq!(CaptureTiming::from_flags(timing.flags()), timing);
            // タイミングは bit0/bit1 (inout/省略可能) と ValueKind の
            // bit2..=4 に触れない
            assert_eq!(timing.flags() & 0b1_1111, 0);
        }
        // 未知のタイミング値は Unspecified として読む (前方互換)
        assert_eq!(
            CaptureTiming::from_flags(7 << CAPTURE_TIMING_SHIFT),
            CaptureTiming::Unspecified
        );
        assert_eq!(CaptureTiming::from_flags(0), CaptureTiming::Unspecified);
        // ValueKind と合成しても互いに読み戻せる
        let both = ValueKind::Handle.flags() | CaptureTiming::After.flags();
        assert_eq!(ValueKind::from_flags(both), ValueKind::Handle);
        assert_eq!(CaptureTiming::from_flags(both), CaptureTiming::After);
    }

    #[test]
    fn capture_timing_changes_content_hash() {
        // 取得タイミングが違う Schema は別物として判定される (flags は
        // content_hash の入力に含まれる)。
        let before = make_schema(
            1,
            "a!B/enter",
            vec![(
                "data".into(),
                Ty::Bytes,
                ValueKind::Scalar.flags() | CaptureTiming::Before.flags(),
            )],
        );
        let after = make_schema(
            1,
            "a!B/enter",
            vec![(
                "data".into(),
                Ty::Bytes,
                ValueKind::Scalar.flags() | CaptureTiming::After.flags(),
            )],
        );
        assert_ne!(before.content_hash, after.content_hash);
    }

    #[test]
    fn last_error_meaning_flags_roundtrip() {
        // F20: GetLastError の生値が「API 仕様上意味のあるエラー値」で
        // ある条件を Schema で宣言する。
        for meaning in [
            LastErrorMeaning::Unspecified,
            LastErrorMeaning::Always,
            LastErrorMeaning::WhenReturnIsFalse,
            LastErrorMeaning::WhenReturnIsInvalidHandle,
            LastErrorMeaning::WhenReturnIsNull,
            LastErrorMeaning::Never,
            LastErrorMeaning::WhenReturnIsMinusOne,
            LastErrorMeaning::WhenReturnIsNotNull,
            LastErrorMeaning::DependsOnInputs,
            LastErrorMeaning::WhenReturnIsNullOrInvalidHandle,
        ] {
            assert_eq!(LastErrorMeaning::from_flags(meaning.flags()), meaning);
            // 意味条件は bit0..=7 (inout/省略可能・ValueKind・
            // CaptureTiming) に触れない
            assert_eq!(meaning.flags() & 0xff, 0);
        }
        assert_eq!(
            LastErrorMeaning::from_flags(0),
            LastErrorMeaning::Unspecified
        );
        // ValueKind・CaptureTiming と合成しても互いに読み戻せる
        let combined = ValueKind::Scalar.flags()
            | CaptureTiming::After.flags()
            | LastErrorMeaning::WhenReturnIsFalse.flags();
        assert_eq!(ValueKind::from_flags(combined), ValueKind::Scalar);
        assert_eq!(CaptureTiming::from_flags(combined), CaptureTiming::After);
        assert_eq!(
            LastErrorMeaning::from_flags(combined),
            LastErrorMeaning::WhenReturnIsFalse
        );
    }

    #[test]
    fn last_error_meaning_changes_content_hash() {
        // 条件が違う Schema は別物として判定される (flags は
        // content_hash の入力に含まれる)。
        let on_false = make_schema(
            1,
            "a!B/leave",
            vec![(
                "last_error".into(),
                Ty::U32,
                ValueKind::Scalar.flags()
                    | CaptureTiming::After.flags()
                    | LastErrorMeaning::WhenReturnIsFalse.flags(),
            )],
        );
        let on_handle = make_schema(
            1,
            "a!B/leave",
            vec![(
                "last_error".into(),
                Ty::U32,
                ValueKind::Scalar.flags()
                    | CaptureTiming::After.flags()
                    | LastErrorMeaning::WhenReturnIsInvalidHandle.flags(),
            )],
        );
        assert_ne!(on_false.content_hash, on_handle.content_hash);
    }

    /// Stale (この呼び出しの LastError ではない) が正しい記録か: Microsoft の規約を言葉どおりに
    /// 書いた表。戻り値 `ret` は記録にあれば Some
    fn stale_is_correct(m: LastErrorMeaning, ret: Option<u64>) -> bool {
        let invalid = |r: u64| r == u64::MAX || r == 0xffff_ffff;
        match m {
            // 意味条件を宣言していない / 常に意味がある: 古い値とは言えない
            LastErrorMeaning::Unspecified | LastErrorMeaning::Always => false,
            // 意味がない API (Sleep、SetLastError..): 戻り値があってもなくても Stale が正しい
            LastErrorMeaning::Never => true,
            // 失敗が 0 の API: 戻り値が取れていて 0 でないときだけ
            LastErrorMeaning::WhenReturnIsFalse => ret.is_some_and(|r| r != 0),
            LastErrorMeaning::WhenReturnIsNull => ret.is_some_and(|r| r != 0),
            // 失敗が -1 / INVALID_HANDLE_VALUE の API
            LastErrorMeaning::WhenReturnIsInvalidHandle => ret.is_some_and(|r| !invalid(r)),
            LastErrorMeaning::WhenReturnIsMinusOne => ret.is_some_and(|r| !invalid(r)),
            // 非 NULL が失敗 (GlobalFree): NULL を返したときだけ
            LastErrorMeaning::WhenReturnIsNotNull => ret == Some(0),
            // NULL も INVALID_HANDLE_VALUE も失敗
            LastErrorMeaning::WhenReturnIsNullOrInvalidHandle => ret.is_some_and(|r| r != 0 && !invalid(r)),
            // 入力で決まる API: leave 単体では戻り値があることだけ (照合は enter と組で)
            LastErrorMeaning::DependsOnInputs => ret.is_some(),
        }
    }

    #[test]
    fn undefined_value_claims_need_a_schema_declaration() {
        // 2026-11-19 レビュー #4/#5: Stale は LastError の条件を宣言したフィールドだけ、
        // NotApplicable は呼び出し後の出力だけが完全性を保てる。
        let meanings = [
            LastErrorMeaning::Unspecified,
            LastErrorMeaning::Always,
            LastErrorMeaning::WhenReturnIsFalse,
            LastErrorMeaning::WhenReturnIsInvalidHandle,
            LastErrorMeaning::WhenReturnIsNull,
            LastErrorMeaning::Never,
            LastErrorMeaning::WhenReturnIsMinusOne,
            LastErrorMeaning::WhenReturnIsNotNull,
            LastErrorMeaning::DependsOnInputs,
            LastErrorMeaning::WhenReturnIsNullOrInvalidHandle,
        ];
        let timings = [
            CaptureTiming::Unspecified,
            CaptureTiming::Before,
            CaptureTiming::After,
            CaptureTiming::Completion,
            CaptureTiming::Both,
        ];
        let conditions = [
            OutputCondition::Unspecified,
            OutputCondition::Always,
            OutputCondition::WhenReturnIsTrue,
            OutputCondition::WhenReturnIsTrueOrMoreData,
            OutputCondition::WhenNotPending,
            OutputCondition::WhenPacketDequeued,
        ];
        let mut facts = vec![ResultFacts::default()];
        for ret in [None, Some(0), Some(1), Some(0x1234), Some(0xffff_ffff), Some(u64::MAX)] {
            for last_error in [None, Some(0), Some(ERROR_MORE_DATA), Some(ERROR_IO_PENDING), Some(5)] {
                for dequeued in [None, Some(0), Some(0x1000)] {
                    facts.push(ResultFacts { ret, last_error, dequeued });
                }
            }
        }
        for code in 0..=u8::MAX {
            let Some(capture) = ValueCapture::from_code(code) else {
                continue;
            };
            for m in meanings {
                for t in timings {
                    for c in conditions {
                        for f in &facts {
                            let flags = m.flags() | t.flags() | c.flags() | ValueKind::Scalar.flags();
                            let got = capture.is_complete_given(flags, f);
                            match capture {
                                ValueCapture::Captured => assert!(got),
                                // 古い値と言えるのは、宣言した条件が実際の結果で偽のときだけ。
                                // 期待値は製品の rule() / is_meaningful を使わない独立した表
                                // (d4fc3c0 レビュー 5: 期待値側が「Never にも戻り値が要る」式を
                                // 写していて、Sleep を不完全とする誤りがテスト上も正解になった)
                                ValueCapture::Stale => assert_eq!(got, stale_is_correct(m, f.ret), "{m:?} {f:?}"),
                                // 値がないと言えるのは、呼び出し後の出力で、その結果では定義されないときだけ
                                ValueCapture::NotApplicable => assert_eq!(
                                    got,
                                    matches!(t, CaptureTiming::After | CaptureTiming::Completion | CaptureTiming::Both)
                                        && c.defined(f) == Some(false),
                                    "{t:?} {c:?} {f:?}"
                                ),
                                ValueCapture::Unreadable
                                | ValueCapture::Truncated
                                | ValueCapture::Unsupported
                                | ValueCapture::LimitExceeded => assert!(!got),
                            }
                        }
                    }
                }
            }
        }
    }

    /// 7c5a553 レビュー P2-1 の組み合わせ (CloseHandle の leave): 戻り値が FALSE なのに Stale と
    /// 書いた LastError、正常に戻った関数の戻り値そのものを NotApplicable と書いたもの
    #[test]
    fn input_dependent_last_error_is_resolved_from_the_enter() {
        let none = |_: &str| None;
        let disp = |d: u64| move |n: &str| (n == "dwCreationDisposition").then_some(d);
        let r = |f, i: &dyn Fn(&str) -> Option<u64>, ret, e| LastErrorMeaning::resolve_inputs(f, i, ret, e);
        for f in ["CreateFileW", "CreateFileA"] {
            for d in 0..8u64 {
                let want = if matches!(d, 2 | 4) {
                    LastErrorMeaning::Always
                } else {
                    LastErrorMeaning::WhenReturnIsInvalidHandle
                };
                assert_eq!(r(f, &disp(d), 0x1234, 0), want);
            }
            assert_eq!(r(f, &disp(u64::MAX), 0x1234, 0), LastErrorMeaning::DependsOnInputs);
            assert_eq!(r(f, &none, 0x1234, 0), LastErrorMeaning::DependsOnInputs);
        }
        let name = |p: u64| move |n: &str| (n == "lpName").then_some(p);
        for f in ["CreateMutexW", "CreateMutexExW", "CreateEventW", "CreateEventExW", "CreateSemaphoreW"] {
            assert_eq!(r(f, &name(0), 0, 5), LastErrorMeaning::Always);
            assert_eq!(r(f, &name(0), 0x10, 0), LastErrorMeaning::Never);
            assert_eq!(r(f, &name(0x99), 0x10, ERROR_ALREADY_EXISTS), LastErrorMeaning::Always);
            assert_eq!(r(f, &name(0x99), 0x10, 0), LastErrorMeaning::DependsOnInputs);
            assert_eq!(r(f, &none, 0x10, 0), LastErrorMeaning::DependsOnInputs);
        }
        assert_eq!(r("OpenFile", &disp(1), 0x10, 0), LastErrorMeaning::DependsOnInputs);
        // leave 単体の検査は戻り値があることだけを見る (CreateMutexW は lpName 次第で
        // INVALID_HANDLE_VALUE 形の値でも Stale が正しい。残りは呼び出し単位の check_stale_inputs)
        let flags = LastErrorMeaning::DependsOnInputs.flags();
        let facts = |ret| ResultFacts { ret, ..ResultFacts::default() };
        for ret in [0x1234, 0, u64::MAX, 0xffff_ffff] {
            assert!(ValueCapture::Stale.is_complete_given(flags, &facts(Some(ret))));
        }
        assert!(!ValueCapture::Stale.is_complete_given(flags, &facts(None)));
    }

    #[test]
    fn stale_and_not_applicable_are_checked_against_the_actual_result() {
        let last_error = LastErrorMeaning::WhenReturnIsFalse.flags() | CaptureTiming::After.flags();
        let failed = ResultFacts { ret: Some(0), last_error: Some(6), dequeued: None };
        let ok = ResultFacts { ret: Some(1), last_error: Some(6), dequeued: None };
        assert!(!ValueCapture::Stale.is_complete_given(last_error, &failed));
        assert!(ValueCapture::Stale.is_complete_given(last_error, &ok));
        assert!(!ValueCapture::Stale.is_complete_given(last_error, &ResultFacts::default()));
        let ret = CaptureTiming::After.flags() | ValueKind::Scalar.flags();
        assert!(!ValueCapture::NotApplicable.is_complete_given(ret, &ok));
        assert!(!ValueCapture::NotApplicable.is_complete_given(ret, &failed));
        // ReadFile の data: 部分読み取り (FALSE + ERROR_MORE_DATA) では値がある
        let data = CaptureTiming::After.flags() | OutputCondition::WhenReturnIsTrueOrMoreData.flags();
        let more = ResultFacts { ret: Some(0), last_error: Some(ERROR_MORE_DATA), dequeued: None };
        assert!(!ValueCapture::NotApplicable.is_complete_given(data, &more));
        assert!(ValueCapture::NotApplicable.is_complete_given(data, &failed));
        // GetQueuedCompletionStatus: FALSE でも OVERLAPPED を取り出したなら値がある
        let gqcs = CaptureTiming::After.flags() | OutputCondition::WhenPacketDequeued.flags();
        let failed_io = ResultFacts { ret: Some(0), last_error: Some(38), dequeued: Some(0x1000) };
        let none = ResultFacts { ret: Some(0), last_error: Some(258), dequeued: Some(0) };
        assert!(!ValueCapture::NotApplicable.is_complete_given(gqcs, &failed_io));
        assert!(ValueCapture::NotApplicable.is_complete_given(gqcs, &none));
        assert!(!ValueCapture::NotApplicable.is_complete_given(gqcs, &ResultFacts { dequeued: None, ..none }));
    }

    #[test]
    fn output_condition_flags_round_trip_and_stay_in_their_bits() {
        for c in [
            OutputCondition::Unspecified,
            OutputCondition::Always,
            OutputCondition::WhenReturnIsTrue,
            OutputCondition::WhenReturnIsTrueOrMoreData,
            OutputCondition::WhenNotPending,
            OutputCondition::WhenPacketDequeued,
        ] {
            assert_eq!(OutputCondition::from_flags(c.flags()), c);
            assert_eq!(c.flags() & !OUTPUT_CONDITION_MASK, 0);
            assert_eq!(c.flags() & (VALUE_KIND_MASK | CAPTURE_TIMING_MASK | LAST_ERROR_MEANING_MASK), 0);
        }
    }

    #[test]
    fn last_error_meaningfulness_matrix() {
        // F20: 生値が意味を持つのは、宣言した条件が戻り値で満たされた
        // 場合だけ。成功時の生値は古い残留値であり意味を持たない。
        use LastErrorMeaning::*;
        assert!(Unspecified.is_meaningful(0));
        assert!(Unspecified.is_meaningful(1));
        assert!(Always.is_meaningful(0));
        assert!(Always.is_meaningful(1));
        assert!(
            DependsOnInputs.is_meaningful(1),
            "return-only checks must not discard input-dependent evidence"
        );
        assert!(DependsOnInputs.is_meaningful(u64::MAX));
        assert!(WhenReturnIsFalse.is_meaningful(0));
        assert!(!WhenReturnIsFalse.is_meaningful(1));
        assert!(WhenReturnIsInvalidHandle.is_meaningful(u64::MAX));
        // 32bit 形の INVALID_HANDLE_VALUE (WoW64。レビュー第5回 #5)
        assert!(WhenReturnIsInvalidHandle.is_meaningful(0x0000_0000_ffff_ffff));
        assert!(!WhenReturnIsInvalidHandle.is_meaningful(0x1234));
        assert!(!WhenReturnIsInvalidHandle.is_meaningful(0));
        // L03: 生成 API 用の追加条件。NULL 返し (HANDLE/ポインタ)、
        // 戻り値がエラーコードそのもの (Reg* 等)、-1 返し (WAIT_FAILED 等)。
        assert!(WhenReturnIsNull.is_meaningful(0));
        assert!(!WhenReturnIsNull.is_meaningful(1));
        assert!(!Never.is_meaningful(0));
        assert!(!Never.is_meaningful(1));
        assert!(WhenReturnIsMinusOne.is_meaningful(u64::MAX));
        // 32bit 形の -1 (WAIT_FAILED 等。レビュー第5回 #5 と同じ幅の扱い)
        assert!(WhenReturnIsMinusOne.is_meaningful(0x0000_0000_ffff_ffff));
        assert!(!WhenReturnIsMinusOne.is_meaningful(0));
        assert!(!WhenReturnIsMinusOne.is_meaningful(0x1234));
        // P3-16: HANDLE returns without a declared failure value.
        assert!(WhenReturnIsNullOrInvalidHandle.is_meaningful(0));
        assert!(WhenReturnIsNullOrInvalidHandle.is_meaningful(u64::MAX));
        assert!(WhenReturnIsNullOrInvalidHandle.is_meaningful(0x0000_0000_ffff_ffff));
        assert!(!WhenReturnIsNullOrInvalidHandle.is_meaningful(0x1234));
    }

    #[test]
    fn stale_capture_code_roundtrip() {
        // F20: 取得状態 Stale (5)。値は読めたが生値が古い
        // （この呼び出しのエラーではない）ことを表す。値本体は生値を
        // 保持する（再検証のため）が、比較・表示では意味のあるエラー値
        // として扱ってはならない。
        assert_eq!(ValueCapture::Stale.code(), 5);
        assert_eq!(ValueCapture::from_code(5), Some(ValueCapture::Stale));
        assert_eq!(
            ValueCapture::from_code(6),
            Some(ValueCapture::NotApplicable)
        );
        assert_eq!(ValueCapture::from_code(7), None);
    }
}
