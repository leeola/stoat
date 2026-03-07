use crate::{
    fs::{Fs, RealFs},
    git::provider::{GitProvider, RealGitProvider},
};
use std::sync::Arc;

pub struct Services {
    pub fs: Arc<dyn Fs>,
    pub git: Arc<dyn GitProvider>,
}

impl Services {
    pub fn production() -> Arc<Self> {
        Arc::new(Self {
            fs: Arc::new(RealFs),
            git: Arc::new(RealGitProvider),
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fake() -> Arc<Self> {
        use crate::{fs::FakeFs, git::provider::FakeGitProvider};

        Arc::new(Self {
            fs: Arc::new(FakeFs::new()),
            git: Arc::new(FakeGitProvider::empty()),
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fake_fs(&self) -> &crate::fs::FakeFs {
        self.fs
            .as_any()
            .downcast_ref::<crate::fs::FakeFs>()
            .expect("Services::fake_fs() called on non-fake Services")
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn fake_git(&self) -> &crate::git::provider::FakeGitProvider {
        self.git
            .as_any()
            .downcast_ref::<crate::git::provider::FakeGitProvider>()
            .expect("Services::fake_git() called on non-fake Services")
    }
}
