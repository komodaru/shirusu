//! scripts/ci/review-mutations.py の表が実際の木と食い違わないこと。
//! あの表は夜間 CI でしか動かないので、テスト名の変更や置換元の文字列の変更で
//! 「baseline failed」になっても push の CI では気づけない (2026-11-20、uncaptured_dummy)。
use super::{Hits, Tree};

pub(super) const SCRIPT: &str = "scripts/ci/review-mutations.py";

#[derive(Debug, PartialEq, Eq)]
enum Tok {
    Open,
    Close,
    Str(String),
}

/// Python の一重引用符の文字列と括弧だけを拾う (表の外は読まない)
fn tokens(text: &str) -> Result<Vec<Tok>, String> {
    let start = text.find("cases = [").ok_or("cases = [ がない")?;
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut chars = text[start + "cases = ".len()..].chars();
    while let Some(c) = chars.next() {
        match c {
            '[' | '(' => {
                depth += 1;
                out.push(Tok::Open);
            }
            ']' | ')' => {
                depth = depth.checked_sub(1).ok_or("括弧が閉じすぎ")?;
                out.push(Tok::Close);
                if depth == 0 {
                    return Ok(out);
                }
            }
            '\'' => {
                let mut s = String::new();
                loop {
                    match chars.next().ok_or("文字列が閉じない")? {
                        '\'' => break,
                        '\\' => match chars.next().ok_or("文字列が閉じない")? {
                            'n' => s.push('\n'),
                            't' => s.push('\t'),
                            '\\' => s.push('\\'),
                            '\'' => s.push('\''),
                            '"' => s.push('"'),
                            other => return Err(format!("未対応のエスケープ \\{other}")),
                        },
                        other => s.push(other),
                    }
                }
                out.push(Tok::Str(s));
            }
            '"' => return Err("表の文字列は一重引用符で書く".into()),
            _ => {}
        }
    }
    Err("cases の表が閉じない".into())
}

#[derive(Debug)]
struct Case {
    label: String,
    file: String,
    replacements: Vec<(String, String)>,
    package: String,
    test: String,
    filter: String,
}

struct Cursor(std::vec::IntoIter<Tok>);

impl Cursor {
    fn open(&mut self) -> Result<(), String> {
        match self.0.next() {
            Some(Tok::Open) => Ok(()),
            t => Err(format!("( か [ がほしいところに {t:?}")),
        }
    }
    fn string(&mut self) -> Result<String, String> {
        match self.0.next() {
            Some(Tok::Str(s)) => Ok(s),
            t => Err(format!("文字列がほしいところに {t:?}")),
        }
    }
    fn close(&mut self) -> Result<(), String> {
        match self.0.next() {
            Some(Tok::Close) => Ok(()),
            t => Err(format!(") か ] がほしいところに {t:?}")),
        }
    }
}

fn cases(text: &str) -> Result<Vec<Case>, String> {
    let mut cur = Cursor(tokens(text)?.into_iter());
    cur.open()?;
    let mut out = Vec::new();
    loop {
        match cur.0.next() {
            Some(Tok::Open) => {}
            Some(Tok::Close) => return Ok(out),
            t => return Err(format!("case の ( がほしいところに {t:?}")),
        }
        let label = cur.string()?;
        let file = cur.string()?;
        cur.open()?;
        let mut replacements = Vec::new();
        loop {
            match cur.0.next() {
                Some(Tok::Open) => {}
                Some(Tok::Close) => break,
                t => return Err(format!("{label}: 置換の ( がほしいところに {t:?}")),
            }
            let old = cur.string()?;
            let new = cur.string()?;
            cur.close()?;
            replacements.push((old, new));
        }
        let package = cur.string()?;
        let test = cur.string()?;
        let filter = cur.string()?;
        cur.close()?;
        out.push(Case {
            label,
            file,
            replacements,
            package,
            test,
            filter,
        });
    }
}

/// 名前に filter を含む有効な #[test] の数 (cargo test の絞り込みは部分一致)
fn matching_tests(items: &[syn::Item], filter: &str, prefix: &str, n: &mut usize) {
    for item in items {
        match item {
            syn::Item::Fn(f)
                if f.attrs.iter().any(|a| a.path().is_ident("test"))
                    && !f.attrs.iter().any(|a| a.path().is_ident("ignore")) =>
            {
                if format!("{prefix}{}", f.sig.ident).contains(filter) {
                    *n += 1;
                }
            }
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    matching_tests(inner, filter, &format!("{prefix}{}::", m.ident), n);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn mutation_cases(tree: &Tree) -> Hits {
    let mut hits = Hits::new();
    if !tree.has_or_real(SCRIPT) {
        return hits;
    }
    let cases = match cases(&tree.text(SCRIPT).replace("\r\n", "\n")) {
        Ok(c) if !c.is_empty() => c,
        Ok(_) => {
            hits.push((SCRIPT.into(), 1, "cases の表が空".into()));
            return hits;
        }
        Err(e) => {
            hits.push((SCRIPT.into(), 1, e));
            return hits;
        }
    };
    let paths = tree.paths();
    for c in cases {
        let line = tree
            .text(SCRIPT)
            .lines()
            .position(|l| l.contains(&format!("('{}',", c.label)))
            .map_or(1, |i| i + 1);
        if !paths.contains(&c.file) {
            hits.push((
                SCRIPT.into(),
                line,
                format!("{}: {} がない", c.label, c.file),
            ));
        } else {
            // Windows の checkout は CRLF になる。夜間の表は Linux で動くので、改行を LF に揃えて数える
            let text = tree.text(&c.file).replace("\r\n", "\n");
            for (old, _) in &c.replacements {
                let n = text.matches(old.as_str()).count();
                if n != 1 {
                    hits.push((
                        SCRIPT.into(),
                        line,
                        format!(
                            "{}: 置換元が {} に {n} 回ある (1 回であること): {old:?}",
                            c.label, c.file
                        ),
                    ));
                }
            }
        }
        let test_file = format!("crates/{}/tests/{}.rs", c.package, c.test);
        if !paths.contains(&test_file) {
            hits.push((
                SCRIPT.into(),
                line,
                format!("{}: {test_file} がない", c.label),
            ));
            continue;
        }
        let mut n = 0;
        match syn::parse_file(&tree.text(&test_file)) {
            Ok(ast) => matching_tests(&ast.items, &c.filter, "", &mut n),
            Err(e) => {
                hits.push((test_file.clone(), super::line(e.span()), e.to_string()));
                continue;
            }
        }
        if n != 1 {
            hits.push((
                SCRIPT.into(),
                line,
                format!(
                    "{}: {test_file} で名前に {:?} を含む有効な #[test] が {n} 個 (1 個であること)",
                    c.label, c.filter
                ),
            ));
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_real_table_parses_with_every_field() {
        let text = std::fs::read_to_string(super::super::root().join(SCRIPT)).unwrap();
        let cases = cases(&text).unwrap();
        assert!(cases.len() >= 10, "{}", cases.len());
        let wide = cases.iter().find(|c| c.label == "wide_abi").unwrap();
        assert!(wide.replacements[0]
            .0
            .contains("}\n    generated_detour(86,"));
        let process = cases.iter().find(|c| c.label == "process").unwrap();
        assert_eq!(process.replacements.len(), 2);
    }

    #[test]
    fn crlf_checkouts_count_replacements_like_lf() {
        let mut tree = super::super::Tree::repo(&super::super::root());
        for file in [SCRIPT, "crates/shirusu-hook/src/engine_generated.inc"] {
            let crlf = tree.text(file).replace("\r\n", "\n").replace('\n', "\r\n");
            tree.overlay.insert(file.into(), Some(crlf));
        }
        assert!(
            mutation_cases(&tree).is_empty(),
            "{:?}",
            mutation_cases(&tree)
        );
    }

    #[test]
    fn malformed_tables_are_errors_not_empty() {
        for bad in [
            "cases = [\n    ('a', 'f', [('x', 'y')], 'p', 't'\n",
            "cases = [\n    ('a', 'f', [('x', 'y')], 'p', 't', \"n\"),\n]\n",
            "cases = [\n    ('a', 'f', [('x')], 'p', 't', 'n'),\n]\n",
            "no table here",
        ] {
            assert!(cases(bad).is_err(), "{bad}");
        }
    }
}
