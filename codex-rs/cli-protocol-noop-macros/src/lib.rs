//! No-op schema derives for the internal CLI protocol.
//!
//! Normal builds retain schema annotations for readable protocol definitions
//! without linking schema generators into the runtime.

use proc_macro::TokenStream;

/// Accepts `#[schemars(...)]` helper attributes without generating an impl.
#[proc_macro_derive(JsonSchema, attributes(schemars))]
pub fn derive_json_schema(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// Accepts `#[ts(...)]` helper attributes without generating an impl.
#[proc_macro_derive(TS, attributes(ts))]
pub fn derive_ts(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
