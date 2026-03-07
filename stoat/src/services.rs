use crate::{
    claude::provider::{ClaudeProvider, RealClaudeProvider},
    fs::{Fs, RealFs},
    git::provider::{GitProvider, RealGitProvider},
};
use std::sync::Arc;

pub struct Services {
    pub fs: Arc<dyn Fs>,
    pub git: Arc<dyn GitProvider>,
    pub claude: Arc<dyn ClaudeProvider>,
}

impl Services {
    pub fn production() -> Arc<Self> {
        Arc::new(Self {
            fs: Arc::new(RealFs),
            git: Arc::new(RealGitProvider),
            claude: Arc::new(RealClaudeProvider),
        })
    }

    #[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
    pub fn fake() -> Arc<Self> {
        use crate::{
            claude::provider::FakeClaudeProvider, fs::FakeFs, git::provider::FakeGitProvider,
        };

        let fs = Arc::new(FakeFs::new());
        Arc::new(Self {
            git: Arc::new(FakeGitProvider::new(fs.clone())),
            fs,
            claude: Arc::new(FakeClaudeProvider::new()),
        })
    }

    #[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
    pub fn fake_fs(&self) -> &crate::fs::FakeFs {
        self.fs
            .as_any()
            .downcast_ref::<crate::fs::FakeFs>()
            .expect("Services::fake_fs() called on non-fake Services")
    }

    #[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
    pub fn fake_git(&self) -> &crate::git::provider::FakeGitProvider {
        self.git
            .as_any()
            .downcast_ref::<crate::git::provider::FakeGitProvider>()
            .expect("Services::fake_git() called on non-fake Services")
    }

    #[cfg(any(test, feature = "test-support", feature = "dev-tools"))]
    pub fn fake_claude(&self) -> &crate::claude::provider::FakeClaudeProvider {
        self.claude
            .as_any()
            .downcast_ref::<crate::claude::provider::FakeClaudeProvider>()
            .expect("Services::fake_claude() called on non-fake Services")
    }
}
