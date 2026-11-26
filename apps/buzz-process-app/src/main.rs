//! The app entry point. The harness resolves the injected context and hands us
//! an `AppContext`; everything else is `buzz_process_app::run`.

#![forbid(unsafe_code)]

use std::io;

use ferrite_app::{AppContext, FerriteApp};

struct ProcessCell;

impl FerriteApp for ProcessCell {
    async fn run(ctx: AppContext) -> io::Result<()> {
        buzz_process_app::run(ctx).await
    }
}

fn main() {
    ferrite_app::app_main::<ProcessCell>()
}
