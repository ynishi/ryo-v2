//! macro-lab `lab-macros`: one derive and two attribute macros, written on
//! `proc_macro` alone (no syn, no quote) so that the fixture builds offline.

use proc_macro::{TokenStream, TokenTree};

/// The identifier after `struct` in the item.
fn struct_name(input: TokenStream) -> Option<String> {
    let mut tokens = input.into_iter();
    while let Some(t) = tokens.next() {
        if let TokenTree::Ident(i) = &t {
            if i.to_string() == "struct" {
                if let Some(TokenTree::Ident(name)) = tokens.next() {
                    return Some(name.to_string());
                }
            }
        }
    }
    None
}

/// `#[derive(Describe)]` adds `pub fn describe(&self) -> String` returning the struct name.
#[proc_macro_derive(Describe)]
pub fn derive_describe(input: TokenStream) -> TokenStream {
    let name = struct_name(input).unwrap_or_else(|| String::from("Unknown"));
    format!("impl {name} {{ pub fn describe(&self) -> String {{ String::from(\"{name}\") }} }}")
        .parse()
        .expect("generated impl parses")
}

/// `#[keep]` returns the item unchanged: every token keeps its own span.
#[proc_macro_attribute]
pub fn keep(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// `#[reparse]` prints the item and parses it again: every token gets the call-site span.
#[proc_macro_attribute]
pub fn reparse(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item.to_string().parse().expect("item reparses")
}
