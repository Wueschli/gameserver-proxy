//! Structural limits checked before a module is compiled.
//!
//! Compiling a module costs time and memory in proportion to its structure, and the
//! controller compiles modules from registries it does not vet. This walk uses
//! `wasmparser` only (no code generation) and rejects a module whose declared structure
//! is out of proportion to any real plugin, naming the bound it hit.

use wasmparser::{Operator, Parser, Payload};

use crate::module::ModuleError;

/// Limits on what a module may declare.
#[derive(Debug, Clone)]
pub struct Bounds {
    pub max_functions: u32,
    pub max_types: u32,
    pub max_locals_per_function: u32,
    pub max_nesting_depth: u32,
    pub max_table_elements: u64,
    pub max_memory_pages: u64,
    pub max_globals: u32,
    pub max_imports: u32,
    pub max_exports: u32,
    pub max_code_bytes: usize,
}

impl Default for Bounds {
    fn default() -> Self {
        Self {
            max_functions: 5_000,
            max_types: 1_000,
            max_locals_per_function: 1_000,
            max_nesting_depth: 200,
            max_table_elements: 100_000,
            max_memory_pages: 1_024,
            max_globals: 1_000,
            max_imports: 16,
            max_exports: 1_000,
            max_code_bytes: 4 * 1024 * 1024,
        }
    }
}

fn too_complex(
    what: &str,
    got: impl std::fmt::Display,
    max: impl std::fmt::Display,
) -> ModuleError {
    ModuleError::TooComplex(format!("{what}: {got} (limit {max})"))
}

fn malformed(e: &wasmparser::BinaryReaderError) -> ModuleError {
    ModuleError::Compile(e.to_string())
}

/// Check `bytes` against `bounds`. Nothing is compiled.
pub fn check(bytes: &[u8], bounds: &Bounds) -> Result<(), ModuleError> {
    let mut functions: u64 = 0;
    let mut code_bytes: usize = 0;
    for payload in Parser::new(0).parse_all(bytes) {
        match payload.map_err(|e| malformed(&e))? {
            Payload::TypeSection(r) if r.count() > bounds.max_types => {
                return Err(too_complex("types", r.count(), bounds.max_types));
            }
            Payload::ImportSection(r) if r.count() > bounds.max_imports => {
                return Err(too_complex("imports", r.count(), bounds.max_imports));
            }
            Payload::ExportSection(r) if r.count() > bounds.max_exports => {
                return Err(too_complex("exports", r.count(), bounds.max_exports));
            }
            Payload::GlobalSection(r) if r.count() > bounds.max_globals => {
                return Err(too_complex("globals", r.count(), bounds.max_globals));
            }
            Payload::FunctionSection(r) => {
                functions += u64::from(r.count());
                if functions > u64::from(bounds.max_functions) {
                    return Err(too_complex("functions", functions, bounds.max_functions));
                }
            }
            Payload::TableSection(r) => {
                for t in r {
                    let t = t.map_err(|e| malformed(&e))?;
                    if t.ty.initial > bounds.max_table_elements {
                        return Err(too_complex(
                            "table elements",
                            t.ty.initial,
                            bounds.max_table_elements,
                        ));
                    }
                }
            }
            Payload::MemorySection(r) => {
                for m in r {
                    let m = m.map_err(|e| malformed(&e))?;
                    if m.initial > bounds.max_memory_pages {
                        return Err(too_complex(
                            "memory pages",
                            m.initial,
                            bounds.max_memory_pages,
                        ));
                    }
                }
            }
            Payload::CodeSectionEntry(body) => {
                code_bytes = code_bytes.saturating_add(body.range().len());
                if code_bytes > bounds.max_code_bytes {
                    return Err(too_complex("code bytes", code_bytes, bounds.max_code_bytes));
                }
                let mut locals: u64 = 0;
                for l in body.get_locals_reader().map_err(|e| malformed(&e))? {
                    let (n, _) = l.map_err(|e| malformed(&e))?;
                    locals += u64::from(n);
                    if locals > u64::from(bounds.max_locals_per_function) {
                        return Err(too_complex(
                            "locals in one function",
                            locals,
                            bounds.max_locals_per_function,
                        ));
                    }
                }
                let mut depth: u32 = 0;
                for op in body.get_operators_reader().map_err(|e| malformed(&e))? {
                    match op.map_err(|e| malformed(&e))? {
                        Operator::Block { .. }
                        | Operator::Loop { .. }
                        | Operator::If { .. }
                        | Operator::Try { .. }
                        | Operator::TryTable { .. } => {
                            depth += 1;
                            if depth > bounds.max_nesting_depth {
                                return Err(too_complex(
                                    "block nesting depth",
                                    depth,
                                    bounds.max_nesting_depth,
                                ));
                            }
                        }
                        Operator::End => depth = depth.saturating_sub(1),
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}
