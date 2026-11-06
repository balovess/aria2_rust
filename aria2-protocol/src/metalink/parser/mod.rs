mod model;
mod parse;
mod selection;
#[cfg(test)]
mod tests;

pub use model::{
    HashAlgorithm, HashEntry, MediaType, MetaUrlEntry, MetalinkDocument, MetalinkFile,
    MetalinkVersion, PieceInfo, UrlEntry,
};
pub use parse::resolve_url;
pub use selection::group_entry_by_metaurl_name;
