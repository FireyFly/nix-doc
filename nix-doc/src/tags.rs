use std::collections::HashMap;
use std::env::current_dir;
use std::fmt::Write;
use std::sync::mpsc::channel;
use std::time::Instant;
use std::{
    fmt, fs, io,
    iter::FromIterator,
    path::{Path, PathBuf},
};

use rnix::types::Inherit;
use rnix::SyntaxNode;
use rnix::{
    types::{AttrSet, EntryHolder, Ident, TokenWrapper, TypedNode},
    SmolStr,
    SyntaxKind::*,
    AST,
};
use walkdir::WalkDir;

use crate::threadpool::ThreadPool;
use crate::{is_ignored, is_searchable};

const DEBUG_TIMERS: bool = false;

struct Timer(Instant);
impl Timer {
    fn new() -> Self {
        Self(Instant::now())
    }

    fn debug_print(&self, name: &str) {
        if DEBUG_TIMERS {
            let time = self.0.elapsed();
            eprintln!(
                "{}: {:0.4} ms",
                name,
                time.as_millis() as f64 + time.subsec_millis() as f64 / 1000.
            );
        }
    }
}

#[derive(Clone, Debug)]
enum MemoValue<T> {
    Uncomputed,
    Failed,
    Value(T),
}

impl<T> Default for MemoValue<T> {
    fn default() -> Self {
        Self::Uncomputed
    }
}

impl<T> MemoValue<T> {
    fn get_or_compute<F>(&mut self, f: F) -> Option<&T>
    where
        F: FnOnce() -> Option<T>,
    {
        match self {
            MemoValue::Uncomputed => {
                *self = f().map(MemoValue::Value).unwrap_or(MemoValue::Failed);
                if let MemoValue::Value(ref v) = self {
                    Some(v)
                } else {
                    None
                }
            }
            MemoValue::Failed => None,
            MemoValue::Value(ref v) => Some(v),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Kind {
    Function,
    Member,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyntacticKind {
    Assign = 0,
    Inherit = 1,
}

impl PartialOrd for SyntacticKind {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        (*self as u32).partial_cmp(&(*other as u32))
    }
}

impl Ord for SyntacticKind {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

/// Path interned in an array of all the paths.
#[derive(Clone, Copy, Debug)]
struct InternedPath(usize);

macro_rules! impl_from {
    ($on:ty, $variant:ident, $ty:ty) => {
        impl From<$ty> for $on {
            fn from(f: $ty) -> $on {
                <$on>::$variant(f)
            }
        }
    };
}

#[derive(Debug)]
pub enum Error {
    Io(io::Error),
}

impl_from!(Error, Io, io::Error);

/// One ctags file entry
struct Tag {
    /// Name of the identifier
    name: SmolStr,

    /// Path relative to the tags file parent dir
    path: InternedPath,

    /// "address" of the tag, the line it's on, basically.
    addr: SmolStr,

    /// Kind of tag
    kind: Kind,

    /// Syntactic type of the tag
    syntactic: SyntacticKind,
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Kind::Function => write!(f, "f"),
            Kind::Member => write!(f, "m"),
        }
    }
}

fn escape(a: &str) -> String {
    let magics = ['\\', '/', '$', '^'];

    let mut result = String::new();
    for c in a.chars() {
        if magics.contains(&c) {
            result.push('\\');
        }
        result.push(c);
    }
    result
}

fn make_addr(a: &str) -> SmolStr {
    // FIXME: delete this cloned malarkey when we can tell everyone with old nixpkgs to go eat a
    // nixpkgs-unstable cookie
    SmolStr::from_iter(["/^", &escape(a), "$/"].iter().cloned())
}

impl Tag {
    fn to_string_relative_to(
        &self,
        paths: &[PathBuf],
        p: &Path,
        memo: &mut Vec<MemoValue<PathBuf>>,
        out: &mut String,
    ) -> Option<()> {
        let relpath =
            memo[self.path.0].get_or_compute(|| pathdiff::diff_paths(&paths[self.path.0], p))?;

        write!(
            out,
            "{}\t{}\t{};\"\t{}",
            self.name,
            relpath.display(),
            make_addr(&self.addr),
            self.kind
        )
        .ok()?;
        Some(())
    }
}

struct FileJob<'a> {
    file: InternedPath,
    source: &'a str,
    results: &'a mut Vec<Tag>,
}

impl<'a> FileJob<'a> {
    fn get_source_line(&self, node: &SyntaxNode) -> &str {
        let defined_at_start = node.text_range().start().to_usize();
        let prior = &self.source[..defined_at_start];
        let line_start = prior.rfind('\n').unwrap_or(0);
        let after = &self.source[defined_at_start..];
        let line_end = after
            .find('\n')
            .unwrap_or(self.source.len() - defined_at_start);
        let source_line = &self.source[line_start..defined_at_start + line_end];
        source_line.strip_prefix('\n').unwrap_or(source_line)
    }

    fn visit_attrset(&mut self, set: &AttrSet) {
        for ent in set.entries() {
            let tag = (|| {
                let val = ent.value()?;
                let key = ent.key()?;

                let kind = match val.kind() {
                    NODE_LAMBDA => Kind::Function,
                    _ => Kind::Member,
                };

                let source_line = self.get_source_line(key.node());

                let ident = key.path().last().and_then(Ident::cast);
                let ident_name = ident.as_ref().map(|id| id.as_str())?;

                Some(Tag {
                    name: ident_name.into(),
                    path: self.file.clone(),
                    addr: source_line.into(),
                    kind,
                    syntactic: SyntacticKind::Assign,
                })
            })();

            if let Some(tag) = tag {
                self.results.push(tag);
            }
        }
    }

    fn visit_inherit(&mut self, inh: &Inherit) {
        for id in inh.idents() {
            (|| {
                let name = id.as_str();
                self.results.push(Tag {
                    name: name.into(),
                    path: self.file.clone(),
                    addr: self.get_source_line(id.node()).into(),
                    kind: Kind::Member,
                    syntactic: SyntacticKind::Inherit,
                });
            })();
        }
    }

    fn exec(&mut self, ast: &AST) {
        for evt in ast.node().preorder_with_tokens() {
            match evt {
                rnix::WalkEvent::Enter(ent) => {
                    ent.into_node().map(|n| match n.kind() {
                        NODE_ATTR_SET => self.visit_attrset(&AttrSet::cast(n).unwrap()),
                        NODE_INHERIT => self.visit_inherit(&Inherit::cast(n).unwrap()),
                        _ => (),
                    });
                }
                rnix::WalkEvent::Leave(_) => (),
            }
        }
    }

    /// Runs a file job collecting tags for a path.
    ///
    /// `p` must be absolute.
    pub fn run(p_interned: InternedPath, p: &Path) -> Result<Vec<Tag>, Error> {
        assert!(p.is_absolute());
        let contents = fs::read_to_string(p)?;
        let parsed = rnix::parse(&contents);
        let mut results = Vec::new();

        let mut job = FileJob {
            file: p_interned,
            source: &contents,
            results: &mut results,
        };

        job.exec(&parsed);

        // we sort here because the rust sorting algo is supposedly good at a bunch of concatenated
        // sorted lists, and parallel compute is effectively free
        results.sort_unstable_by(|e1, e2| {
            e1.name
                .as_str()
                .cmp(e2.name.as_str())
                .then_with(|| e1.syntactic.cmp(&e2.syntactic))
        });

        Ok(results)
    }
}

/// Writes out the header of the tags file to the writer.
fn write_header(mut writer: impl io::Write) -> Result<(), Error> {
    /*
    !_TAG_FILE_FORMAT	2	/extended format; --format=1 will not append ;" to lines/
    !_TAG_FILE_SORTED	1	/0=unsorted, 1=sorted, 2=foldcase/
    !_TAG_OUTPUT_EXCMD	mixed	/number, pattern, mixed, or combineV2/
    !_TAG_OUTPUT_FILESEP	slash	/slash or backslash/
    !_TAG_OUTPUT_MODE	u-ctags	/u-ctags or e-ctags/
    !_TAG_PATTERN_LENGTH_LIMIT	96	/0 for no limit/
    !_TAG_PROC_CWD	/home/jade/co/neovim/	//
    !_TAG_PROGRAM_AUTHOR	Universal Ctags Team	//
    !_TAG_PROGRAM_NAME	Universal Ctags	/Derived from Exuberant Ctags/
    !_TAG_PROGRAM_URL	https://ctags.io/	/official site/
    !_TAG_PROGRAM_VERSION	5.9.0	/e70d5a8f3/
         */
    writeln!(writer, "!_TAG_FILE_FORMAT\t2\t/extended format/")?;
    writeln!(
        writer,
        "!_TAG_FILE_SORTED\t1\t/0=unsorted, 1=sorted, 2=foldcase/"
    )?;
    writeln!(writer, "!_TAG_FILE_ENCODING\tutf-8\t//")?;
    writeln!(writer, "!_TAG_PROGRAM_NAME\tnix-doc tags\t//")?;
    writeln!(
        writer,
        "!_TAG_PROGRAM_URL\thttps://github.com/lf-/nix-doc\t//"
    )?;
    Ok(())
}

/// Removes high-cardinality tags, since they are most likely useless.
fn run_cardinality(max_cardinality: u32, data: &mut Vec<Tag>) {
    let mut cardinalities = HashMap::new();

    for tag in data.iter() {
        // clone is O(1)
        let cardinality = cardinalities.entry(tag.name.clone()).or_insert(0u32);
        *cardinality += 1;
    }

    data.retain(|d| cardinalities[&d.name] <= max_cardinality);
}

/// Builds a tags database into the given writer with paths relative to the current directory, with
/// the nix files in `dir`
pub fn run_on_dir(
    dir: &Path,
    max_cardinality: Option<u32>,
    mut writer: impl io::Write,
) -> Result<(), Error> {
    let pool = ThreadPool::default();
    let (tx, rx) = channel();

    let mut paths_interned = Vec::new();
    let curdir = current_dir()?;

    //println!("searching {}", dir.display());
    let walk_t = Timer::new();
    for direntry in WalkDir::new(dir)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(|e| e.ok())
        .filter(|e| is_searchable(e.path()) && e.path().is_file())
    {
        let path = curdir.join(direntry.into_path());
        let path_ = path.clone();
        paths_interned.push(path);
        let path_interned = InternedPath(paths_interned.len() - 1);

        let my_tx = tx.clone();
        pool.push(move || {
            let results = FileJob::run(path_interned, &path_);
            let results = match results {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("Error processing {}: {:?}", &path_.display(), e);
                    return;
                }
            };

            if !results.is_empty() {
                my_tx.send(results).expect("failed to send tags");
            }
        });
    }

    drop(tx);
    pool.done();
    walk_t.debug_print("walk time");

    let mut out = Vec::new();
    while let Ok(set) = rx.recv() {
        out.extend(set);
    }

    let sort_t = Timer::new();
    out.sort_by(|e1, e2| e1.name.as_str().cmp(e2.name.as_str()));
    sort_t.debug_print("final sort time");

    let write_t = Timer::new();
    write_header(&mut writer)?;

    if let Some(cardinality) = max_cardinality {
        run_cardinality(cardinality, &mut out)
    }

    let mut memo = vec![MemoValue::Uncomputed; paths_interned.len()];
    let mut out_s = String::new();

    for tag in out {
        out_s.clear();
        match tag.to_string_relative_to(&paths_interned, &curdir, &mut memo, &mut out_s) {
            Some(_) => (),
            None => continue,
        };
        writer.write(out_s.as_bytes())?;
        writer.write(b"\n")?;
    }
    write_t.debug_print("write time");

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{env::current_dir, path::PathBuf};

    use super::*;
    use expect_test::{expect, Expect};

    fn check(dir: &str, max_cardinality: Option<u32>, expected: Expect) {
        let curdir = current_dir().unwrap();
        let dir = curdir.join(PathBuf::from(dir));

        println!("datadir: {}", &dir.display());
        println!("cwd: {:?}", &curdir);
        let mut out = Vec::new();

        run_on_dir(&PathBuf::from("."), max_cardinality, &mut out).unwrap();
        let out_s = std::str::from_utf8(&out).unwrap();
        println!("{}", out_s);

        expected.assert_eq(out_s.trim());
    }

    #[test]
    fn smoke() {
        check(
            "testdata",
            None,
            expect![[r#"
                !_TAG_FILE_FORMAT	2	/extended format/
                !_TAG_FILE_SORTED	1	/0=unsorted, 1=sorted, 2=foldcase/
                !_TAG_FILE_ENCODING	utf-8	//
                !_TAG_PROGRAM_NAME	nix-doc tags	//
                !_TAG_PROGRAM_URL	https://github.com/lf-/nix-doc	//
                c	testdata/test.nix	/^   a.b.c = a: 1;$/;"	f
                c	testdata/test.nix	/^   c = {$/;"	m
                ff	testdata/test.nix	/^   inherit ff;$/;"	m
                fixedWidthString	testdata/regression-11.nix	/^  fixedWidthString = width: filler: str:$/;"	f
                grub	testdata/test.nix	/^   inherit (n) grub hello;$/;"	m
                hello	testdata/test.nix	/^   inherit (n) grub hello;$/;"	m
                the-fn	testdata/test.nix	/^   the-fn = a: b: {z = a; y = b;};$/;"	f
                the-fn	testdata/test.nix	/^    the-fn = a: a;$/;"	f
                the-fn	testdata/test.nix	/^    the-fn = a: a;$/;"	f
                the-fn	testdata/test.nix	/^    inherit the-fn;$/;"	m
                the-snd-fn	testdata/test.nix	/^   the-snd-fn = {b, \/* doc *\/ c}: {};$/;"	f
                withFeature	testdata/regression-11.nix	/^  withFeature = with_: feat: "--\${if with_ then "with" else "without"}-\${feat}";$/;"	f
                withFeatureAs	testdata/regression-11.nix	/^  withFeatureAs = with_: feat: value: withFeature with_ feat + optionalString with_ "=\${value}";$/;"	f
                x	testdata/test.nix	/^   x = {$/;"	m
                y	testdata/test.nix	/^   y = {$/;"	m
                y	testdata/test.nix	/^   the-fn = a: b: {z = a; y = b;};$/;"	m
                z	testdata/test.nix	/^   the-fn = a: b: {z = a; y = b;};$/;"	m"#]],
        );
    }

    #[test]
    fn smoke_cardinality() {
        check(
            "testdata",
            Some(1),
            expect![[r#"
                !_TAG_FILE_FORMAT	2	/extended format/
                !_TAG_FILE_SORTED	1	/0=unsorted, 1=sorted, 2=foldcase/
                !_TAG_FILE_ENCODING	utf-8	//
                !_TAG_PROGRAM_NAME	nix-doc tags	//
                !_TAG_PROGRAM_URL	https://github.com/lf-/nix-doc	//
                ff	testdata/test.nix	/^   inherit ff;$/;"	m
                fixedWidthString	testdata/regression-11.nix	/^  fixedWidthString = width: filler: str:$/;"	f
                grub	testdata/test.nix	/^   inherit (n) grub hello;$/;"	m
                hello	testdata/test.nix	/^   inherit (n) grub hello;$/;"	m
                the-snd-fn	testdata/test.nix	/^   the-snd-fn = {b, \/* doc *\/ c}: {};$/;"	f
                withFeature	testdata/regression-11.nix	/^  withFeature = with_: feat: "--\${if with_ then "with" else "without"}-\${feat}";$/;"	f
                withFeatureAs	testdata/regression-11.nix	/^  withFeatureAs = with_: feat: value: withFeature with_ feat + optionalString with_ "=\${value}";$/;"	f
                x	testdata/test.nix	/^   x = {$/;"	m
                z	testdata/test.nix	/^   the-fn = a: b: {z = a; y = b;};$/;"	m"#]],
        );
    }
}
