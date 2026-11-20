use crate::clvm::program::Program;
use crate::clvm::sexp::AtomBuf;
use crate::clvm::sexp::SExp;
use std::io::Error;
use std::io::ErrorKind;

pub fn concat<'a>(sexps: &'_ [SExp<'a>]) -> Result<SExp<'a>, Error> {
    let mut buf = vec![];
    for sexp in sexps {
        match sexp {
            SExp::Atom(a) => {
                buf.extend(a.as_ref());
            }
            SExp::Pair(_) => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "(internal error) concat expected atom, got pair",
                ));
            }
        }
    }
    Ok(SExp::Atom(AtomBuf::new(buf)))
}

pub fn curry(program: &'_ Program, args: &[Program<'_>]) -> Program<'static> {
    let mut fixed_args = Program::to(1);
    for arg in args.iter().map(Program::sexp).rev() {
        fixed_args = Program::to(&[
            SExp::from(4),
            SExp::from(1).cons(arg.clone()),
            fixed_args.sexp().to_owned(),
        ]);
    }
    Program::to(&[
        SExp::from(2),
        Program::to((1, program.sexp().to_owned()))
            .sexp()
            .to_owned(),
        fixed_args.sexp().to_owned(),
    ])
    .to_owned()
}

// SExp::Pair(
// PairBuf::Owned((SExp::from(1).into(), program.sexp().to_owned().into()))
// ),
