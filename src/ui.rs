//! Utility functions for the UI

use std::io::Write;
use std::collections::BTreeMap;

use anyhow::Result;
use anyhow::Error;
use handlebars::Handlebars;

use crate::package::Package;

pub fn print_packages<'a, I>(out: &mut dyn Write, format: &str, iter: I) -> Result<()>
    where I: Iterator<Item = &'a Package>
{
    let mut hb = Handlebars::new();
    hb.register_template_string("package", format)?;

    for (i, package) in iter.enumerate() {
        print_package(out, &hb, i, package)?;
    }

    Ok(())
}

fn print_package(out: &mut dyn Write, hb: &Handlebars, i: usize, package: &Package) -> Result<()> {
    let mut data = BTreeMap::new();
    data.insert("i", i.to_string());
    data.insert("name",             format!("{}", package.name()));
    data.insert("version",          format!("{}", package.version()));
    data.insert("source_url",       format!("{}", package.source().url()));
    data.insert("source_hash_type", format!("{}", package.source().hash().hashtype()));
    data.insert("source_hash",      format!("{}", package.source().hash().value()));

    hb.render("package", &data)
        .map_err(Error::from)
        .and_then(|r| write!(out, "{}", r).map_err(Error::from))
}