//! `$batch`: many writes to `/tdata` in one request, each answered on its own.

mod rows;
mod run;

pub use run::handle_odata_batch;
