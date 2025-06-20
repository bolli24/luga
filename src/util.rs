pub trait ResultCollectAll<T, E> {
    fn collect_all(self) -> Result<Vec<T>, Vec<E>>;
}

impl<T, E, I> ResultCollectAll<T, E> for I
where
    I: Iterator<Item = Result<T, E>>,
{
    fn collect_all(self) -> Result<Vec<T>, Vec<E>> {
        let mut oks = Vec::new();
        let mut errs = Vec::new();

        for r in self {
            match r {
                Ok(v) => oks.push(v),
                Err(e) => errs.push(e),
            }
        }

        if errs.is_empty() { Ok(oks) } else { Err(errs) }
    }
}
