#[derive(Debug)]
pub(crate) struct Lazy<T>(std::cell::OnceCell<T>);

impl<T> Lazy<T> {
    pub(crate) fn new(_value: impl FnOnce() -> T) -> Self {
        Self(std::cell::OnceCell::new())
    }

    pub(crate) fn get(&self, init: impl FnOnce() -> T) -> &T {
        self.0.get_or_init(init)
    }
}
