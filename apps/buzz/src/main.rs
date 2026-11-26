//! The app entry point. The harness resolves the injected context, builds the
//! runtime, and hands us an `AppContext`; everything else is `buzz::run`.

#![forbid(unsafe_code)]

use std::io;

use ferrite_app::{AppContext, FerriteApp};

struct Buzz;

impl FerriteApp for Buzz {
    async fn run(ctx: AppContext) -> io::Result<()> {
        buzz::run(ctx).await
    }
}

fn main() {
    ferrite_app::app_main::<Buzz>()
}
