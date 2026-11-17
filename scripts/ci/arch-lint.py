#!/usr/bin/env python3
"""構造の lint: レビュー (2026-11-15/16) で型と構造に移した決まりが、次の変更で黙って
戻らないようにテキストで検査する。clippy の deny 設定や trybuild は「その場の 1 式」を
見るが、ここで見るのは「その設定や検査そのものが残っているか」と「どこに何を書いてよいか」。

  python3 scripts/ci/arch-lint.py              検査 (CI: scripts/ci/linux.sh)
  python3 scripts/ci/arch-lint.py --self-test  各規則が、退行を入れた架空の木と、実際の
                                               ファイルに退行を入れた木の両方で発火し、
                                               正しい書き方では発火しないことを確かめる

規則ごとの docstring に、どの変更で決まったか、代わりに何を書くかを書く。
"""
import os, re, subprocess, sys

ROOT = os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
DENY = ["unsafe_op_in_unsafe_fn", "not_unsafe_ptr_arg_deref", "missing_safety_doc", "undocumented_unsafe_blocks"]


def code_only(text):
    """コメントと文字列リテラルを空白にした text (改行は残す: 行番号が変わらない)"""
    out, i, n = [], 0, len(text)
    pat = re.compile(r'//[^\n]*|/\*(?:[^*]|\*(?!/))*\*/|b?r(#*)"(?:.|\n)*?"\1|b?"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\\n])\'', re.S)
    last = 0
    for m in pat.finditer(text):
        out.append(text[last:m.start()])
        out.append("".join(c if c == "\n" else " " for c in m.group(0)))
        last = m.end()
    out.append(text[last:])
    return "".join(out)


class Tree:
    """git の追跡ファイル (files を渡せば架空の木) に overlay (path -> text, None は削除) を重ねたもの"""

    def __init__(self, files=None, overlay=None):
        self.base, self.overlay = files, dict(overlay or {})
        self._paths = None

    def paths(self):
        if self._paths is None:
            if self.base is not None:
                ps = set(self.base)
            else:
                out = subprocess.run(["git", "ls-files", "-z"], cwd=ROOT, stdout=subprocess.PIPE, check=True).stdout
                ps = {p for p in out.decode().split("\0") if p}
            ps |= {p for p, t in self.overlay.items() if t is not None}
            ps -= {p for p, t in self.overlay.items() if t is None}
            self._paths = sorted(ps)
        return self._paths

    def text(self, p):
        if p in self.overlay:
            return self.overlay[p] or ""
        if self.base is not None:
            return self.base.get(p, "")
        try:
            return open(os.path.join(ROOT, p), encoding="utf-8", errors="replace").read()
        except FileNotFoundError:
            return ""


def line_of(s, at):
    return s.count("\n", 0, at) + 1


RULES = []


def rule(bad, good, mutate):
    def wrap(f):
        f.id, f.bad, f.good, f.mutate = f.__name__.replace("_", "-"), bad, good, mutate
        RULES.append(f)
        return f
    return wrap


CRATE_TOML = re.compile(r"^crates/[^/]+/Cargo\.toml$")


@rule(bad={"crates/shirusu-x/Cargo.toml": '[package]\nname = "shirusu-x"\n'},
      good={"crates/shirusu-x/Cargo.toml": '[package]\nname = "shirusu-x"\n\n[lints]\nworkspace = true\n'},
      mutate=("crates/shirusu-transport/Cargo.toml", lambda t: re.sub(r"\[lints\]\s*\nworkspace\s*=\s*true\s*\n?", "", t)))
def lints_inherited(tree):
    """どの crate も workspace の lint (unsafe の deny 4 つ) を継ぐ: `[lints] workspace = true`
    のない crate は deny を受けずに unsafe を書ける (レビュー 2026-11-15)。"""
    for p in tree.paths():
        if CRATE_TOML.match(p) and not re.search(r"^\[lints\]\s*\nworkspace\s*=\s*true", tree.text(p), re.M):
            yield p, 1, "[lints] workspace = true がない"


@rule(bad={"Cargo.toml": '[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = "warn"\n'},
      good={"Cargo.toml": '[workspace.lints.rust]\nunsafe_op_in_unsafe_fn = "deny"\n[workspace.lints.clippy]\nnot_unsafe_ptr_arg_deref = "deny"\nmissing_safety_doc = "deny"\nundocumented_unsafe_blocks = "deny"\n'},
      mutate=("Cargo.toml", lambda t: t.replace('undocumented_unsafe_blocks = "deny"', 'undocumented_unsafe_blocks = "warn"')))
def deny_kept(tree):
    """workspace の Cargo.toml が unsafe の 4 lint を deny のまま持つ (レビュー 2026-11-15)。"""
    src = tree.text("Cargo.toml")
    for l in DENY:
        if not re.search(r"^" + l + r'\s*=\s*"deny"', src, re.M):
            yield "Cargo.toml", 1, f'{l} = "deny" がない'


@rule(bad={"crates/shirusu-x/src/lib.rs": "#[allow(clippy::undocumented_unsafe_blocks)]\nfn f() {}\n#![expect(unsafe_op_in_unsafe_fn)]\n"},
      good={"crates/shirusu-x/src/lib.rs": "// #[allow(clippy::undocumented_unsafe_blocks)] とは書かない\n#[allow(clippy::too_many_arguments)]\nfn f() {}\n"},
      mutate=("crates/shirusu-transport/src/shm.rs", lambda t: t + "\n#[allow(clippy::undocumented_unsafe_blocks)]\nfn _lint_regression() {}\n"))
def no_local_allow(tree):
    """deny した unsafe の lint を `#[allow]` / `#[expect]` で局所的に外さない: 外せば deny は
    ただの注意書きになる。理由は // SAFETY: と # Safety に書く。"""
    rx = re.compile(r"#!?\[\s*(?:allow|expect)\s*\([^\]]*\b(?:clippy::)?(?:" + "|".join(DENY) + r")\b")
    for p in tree.paths():
        if p.endswith(".rs"):
            src = code_only(tree.text(p))
            for m in rx.finditer(src):
                yield p, line_of(src, m.start()), m.group(0)


SHM = ("crates/shirusu-transport/src/shm.rs", "crates/shirusu-transport/src/ring.rs")


@rule(bad={SHM[0]: "impl Shm {\n    pub fn copy_out(&self, r: Range<usize>) -> Option<Vec<u8>> { None }\n    pub fn snapshot(&self) -> Vec<u8> { vec![] }\n    pub fn as_slice(&self) -> &[u8] { &[] }\n}\n"},
      good={SHM[0]: "impl Shm {\n    pub unsafe fn copy_out(&self, r: Range<usize>) -> Option<Vec<u8>> { None }\n    pub unsafe fn snapshot(&self) -> Vec<u8> { vec![] }\n}\nimpl ControlSnapshot {\n    pub fn snapshot(self) -> [u8; 64] { [0; 64] }\n}\n"},
      mutate=(SHM[0], lambda t: t.replace("pub unsafe fn copy_out(", "pub fn copy_out(")))
def shm_raw_unsafe(tree):
    """共有メモリの生コピーは unsafe のまま (レビュー 2026-11-16 P0): `Shm::copy_out` と
    `snapshot` は相手プロセスが書いている最中のバイトを読むので unsafe fn。借用の as_slice は
    共有メモリ側には戻さない (相手の書き込みと競合する)。ControlSnapshot など値の型は別。"""
    for p in SHM:
        src = code_only(tree.text(p))
        for m in re.finditer(r"\bpub\s+fn\s+(?:copy_out\s*\(|snapshot\s*\(\s*&self\s*\)\s*->\s*Vec<u8>)", src):
            yield p, line_of(src, m.start()), m.group(0)
        for m in re.finditer(r"\bimpl\s+(?:<[^>]*>\s*)?(?:Shm|Ring\w*)\b[^{]*\{", src):
            depth, j = 1, m.end()
            while j < len(src) and depth:
                depth += {"{": 1, "}": -1}.get(src[j], 0)
                j += 1
            for f in re.finditer(r"\bfn\s+as_(?:mut_)?slice\b", src[m.end():j]):
                yield p, line_of(src, m.end() + f.start()), "共有メモリの借用 as_slice"


UI_LIST = "scripts/ci/arch-lint-ui.txt"


@rule(bad={UI_LIST: "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs\n", "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.stderr": "e"},
      good={UI_LIST: "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs\n", "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs": "fn main() {}",
            "crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.stderr": "e", "crates/shirusu-transport/tests/compile_fail.rs": 'compile_fail("tests/ui/*.rs")'},
      mutate=("crates/shirusu-transport/tests/ui/copy_out_needs_unsafe.rs", lambda t: None))
def compile_fail_kept(tree):
    """「間違いがコンパイルエラーになる」ことを示す trybuild の fixture は減らない: 一覧
    (scripts/ci/arch-lint-ui.txt) のどれかが消えれば、その型の守りは確かめられなくなる。
    新しい fixture は一覧に足す。各 .rs に .stderr があり、crate の tests が tests/ui を読む。"""
    have = set(tree.paths())
    for i, line in enumerate(tree.text(UI_LIST).splitlines(), 1):
        p = line.strip()
        if not p or p.startswith("#"):
            continue
        if p not in have:
            yield UI_LIST, i, f"{p} がない"
        elif p[:-3] + ".stderr" not in have:
            yield p, 1, ".stderr がない"
        else:
            harness = p.split("/tests/ui/")[0] + "/tests/compile_fail.rs"
            if "tests/ui" not in tree.text(harness):
                yield harness, 1, "tests/ui を読まない"
    for p in sorted(have):
        if re.search(r"/tests/ui/.*\.rs$", p) and p not in tree.text(UI_LIST) and tree.base is None:
            yield p, 1, f"{UI_LIST} にない fixture (足す)"


BUILD_EXT = re.compile(r"\.(?:wasm|exe|dll|so|dylib|o|a|lib|rlib|pdb|zst|tar|tgz|7z|zip)$", re.I)


@rule(bad={"target/debug/shirusu.exe": "x", "dist/shirusu_agent.dll": "x"},
      good={"corpus/x.json": "x", "crates/shirusu-hook/src/target.rs": "x"},
      mutate=("crates/shirusu-agent-dll/target/release/shirusu_agent.dll", lambda t: "x"))
def build_output(tree):
    """ビルド成果物を git に入れない (masu と同じ決まり)。target/ 以下か、コンパイル・
    圧縮済みのファイル。"""
    for p in tree.paths():
        if re.search(r"(^|/)target/", p) or (BUILD_EXT.search(p) and "/testdata/" not in p and not p.startswith("corpus/")):
            yield p, 1, p


REQUIRED = [
    ("scripts/ci/linux.sh", "python3 scripts/ci/arch-lint.py", "この lint が毎回動く"),
    ("scripts/ci/linux.sh", "python3 scripts/ci/arch-lint.py --self-test", "各規則が退行で発火することを毎回確かめる"),
    ("scripts/ci/linux.sh", "cargo clippy --workspace --all-targets -- -D warnings", "deny した lint が CI で効く"),
    ("crates/shirusu-hook/tests/generic_contract.rs", "engine_generated.inc", "生成 Detour 表の契約 (GENERIC_CONTRACT) の golden"),
    ("crates/shirusu-transport/src/shm.rs", "copy_out_is_bounds_checked_and_never_covers_control", "copy_out が Control 範囲を返さないテスト"),
]


@rule(bad={"scripts/ci/linux.sh": "cargo test\n"},
      good={p: "\n".join(t for q, t, _ in REQUIRED if q == p) for p, _, _ in REQUIRED},
      mutate=("scripts/ci/linux.sh", lambda t: t.replace("python3 scripts/ci/arch-lint.py --self-test", "")))
def required(tree):
    """守りそのものが残る: CI の手順、生成表の契約 golden、Control 範囲のテスト。消せば
    検査されていた決まりが願いに戻る。架空の木では持っているファイルだけ見る。"""
    for p, t, why in REQUIRED:
        if tree.base is not None and p not in tree.base:
            continue
        if t not in tree.text(p):
            yield p, 1, f"`{t}` がない: {why}"


def check(tree, rules=None):
    return [(r.id, f, n, w) for r in rules or RULES for f, n, w in r(tree)]


def self_test():
    bad = 0
    for r in RULES:
        if not list(r(Tree(files=r.bad))):
            print(f"arch-lint self-test: {r.id}: 退行の fixture で何も見つけない")
            bad += 1
        if hits := list(r(Tree(files=r.good))):
            print(f"arch-lint self-test: {r.id}: 正しい fixture で発火する: {hits}")
            bad += 1
    if check(Tree()):
        print("arch-lint self-test: 木そのものが通らない (まず検査を通す)")
        bad += 1
    real = Tree()
    for r in RULES:
        f, mut = r.mutate
        if not check(Tree(overlay={f: mut(real.text(f))}), [r]):
            print(f"arch-lint self-test: {r.id}: {f} に入れた退行を見逃す")
            bad += 1
    if bad:
        sys.exit(f"arch-lint self-test: {bad} 件失敗")
    print(f"arch-lint self-test: ok ({len(RULES)} 規則: それぞれ fixture と実ファイルの退行で発火し、正しい書き方では発火しない)")


def main():
    if "--self-test" in sys.argv:
        return self_test()
    found = check(Tree())
    for r, f, n, w in found:
        doc = next(x for x in RULES if x.id == r).__doc__.split("\n")[0]
        print(f"arch-lint: {f}:{n}: {r}: {w}\n    ({doc} ... scripts/ci/arch-lint.py)")
    if found:
        sys.exit(f"arch-lint: {len(found)} 件 (規則ごとの docstring に代わりの書き方がある)")
    print(f"arch-lint: ok ({len(RULES)} 規則)")


if __name__ == "__main__":
    main()
