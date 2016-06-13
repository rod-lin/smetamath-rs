// TODO experiment with FNV hashers, etc.
use std::borrow::Borrow;
use std::hash::Hash;
use std::sync::Arc;
use std::u32;
use parser::{Comparer, Segment, SegmentId, SegmentOrder, SegmentRef, StatementAddress, SymbolType,
             Token, TokenAddress, TokenPtr};
use segment_set::SegmentSet;
use util;
use util::HashMap;
// An earlier version of this module was tasked with detecting duplicate symbol errors;
// current task is just lookup

#[derive(Copy,Clone,Debug,PartialEq,Eq,Default,Hash)]
pub struct Atom(u32);

type NameSlot<A, V> = Vec<(A, V)>;

fn slot_insert<A, C, V>(slot: &mut NameSlot<A, V>, comparer: &C, address: A, value: V)
    where C: Comparer<A>
{
    slot.push((address, value));
    slot.sort_by(|x, y| comparer.cmp(&x.0, &y.0));
}

fn slot_remove<A: Eq, V>(slot: &mut NameSlot<A, V>, address: A) {
    slot.retain(|x| x.0 != address);
}

fn autoviv<K, V: Default>(map: &mut HashMap<K, V>, key: K) -> &mut V
    where K: Hash + Eq
{
    map.entry(key).or_insert_with(Default::default)
}

fn deviv<K, Q: ?Sized, V, F>(map: &mut HashMap<K, V>, key: &Q, fun: F)
    where F: FnOnce(&mut V),
          K: Borrow<Q>,
          Q: Hash + Eq,
          K: Hash + Eq,
          V: Default + Eq
{
    let kill = match map.get_mut(key) {
        None => false,
        Some(rval) => {
            fun(rval);
            *rval == Default::default()
        }
    };
    if kill {
        map.remove(key);
    }
}

#[derive(Default, PartialEq, Eq, Debug, Clone)]
struct SymbolInfo {
    atom: Atom,
    all: NameSlot<TokenAddress, SymbolType>,
    constant: NameSlot<TokenAddress, ()>,
    float: NameSlot<StatementAddress, (Token, Token, Atom)>,
}

#[derive(Default,Debug,Clone)]
struct AtomTable {
    table: HashMap<Token, Atom>,
    reverse: Vec<Token>,
}

fn intern(table: &mut AtomTable, tok: TokenPtr) -> Atom {
    let next = Atom(table.table.len() as u32 + 1);
    match table.table.get(tok) {
        None => {}
        Some(atom) => return *atom,
    };
    table.table.insert(tok.to_owned(), next);
    if table.reverse.len() == 0 {
        table.reverse.push(Token::new());
    }
    table.reverse.push(tok.to_owned());
    next
}

#[derive(Default,Debug,Clone)]
pub struct Nameset {
    atom_table: AtomTable,
    pub order: Arc<SegmentOrder>,

    segments: HashMap<SegmentId, Arc<Segment>>,
    dv_info: NameSlot<StatementAddress, Vec<Atom>>,
    labels: HashMap<Token, NameSlot<StatementAddress, ()>>,
    symbols: HashMap<Token, SymbolInfo>,
}

impl Nameset {
    pub fn new() -> Nameset {
        Nameset::default()
    }

    pub fn update(&mut self, segs: &SegmentSet) {
        self.order = segs.order.clone();

        let mut keys_to_remove = Vec::new();
        for (&seg_id, &ref seg) in &self.segments {
            let stale = match segs.segments.get(&seg_id) {
                None => true,
                Some(seg_new) => !util::ptr_eq::<Segment>(seg_new, seg),
            };

            if stale {
                keys_to_remove.push(seg_id);
            }
        }

        for seg_id in keys_to_remove {
            self.remove_segment(seg_id);
        }

        for (&seg_id, &ref seg) in &segs.segments {
            self.add_segment(seg_id, seg.clone());
        }
    }

    pub fn add_segment(&mut self, id: SegmentId, seg: Arc<Segment>) {
        if self.segments.contains_key(&id) {
            return;
        }

        self.segments.insert(id, seg.clone());
        let sref = SegmentRef {
            segment: &seg,
            id: id,
        };

        for &ref symdef in &seg.symbols {
            let slot = autoviv(&mut self.symbols, symdef.name.clone());
            if slot.atom == Atom::default() {
                slot.atom = intern(&mut self.atom_table, &symdef.name);
            }
            let address = TokenAddress::new3(id, symdef.start, symdef.ordinal);
            slot_insert(&mut slot.all, &*self.order, address, symdef.stype);
            if symdef.stype == SymbolType::Constant {
                slot_insert(&mut slot.constant, &*self.order, address, ());
            }
        }

        for &ref lsymdef in &seg.local_vars {
            let name = sref.statement(lsymdef.index).math_at(lsymdef.ordinal).slice;
            intern(&mut self.atom_table, name);
        }

        for &ref labdef in &seg.labels {
            let label = sref.statement(labdef.index).label().to_owned();
            let slot = autoviv(&mut self.labels, label);
            slot_insert(slot,
                        &*self.order,
                        StatementAddress::new(id, labdef.index),
                        ());
        }

        for &ref floatdef in &seg.floats {
            let slot = autoviv(&mut self.symbols, floatdef.name.clone());
            if slot.atom == Atom::default() {
                slot.atom = intern(&mut self.atom_table, &floatdef.name);
            }
            let address = StatementAddress::new(id, floatdef.start);
            let tcatom = intern(&mut self.atom_table, &floatdef.typecode);
            slot_insert(&mut slot.float,
                        &*self.order,
                        address,
                        (floatdef.label.clone(), floatdef.typecode.clone(), tcatom));
        }

        for &ref dvdef in &seg.global_dvs {
            let vars = dvdef.vars.iter().map(|v| intern(&mut self.atom_table, &v)).collect();
            slot_insert(&mut self.dv_info,
                        &*self.order,
                        StatementAddress::new(id, dvdef.start),
                        vars);
        }
    }

    pub fn remove_segment(&mut self, id: SegmentId) {
        if let Some(seg) = self.segments.remove(&id) {
            let sref = SegmentRef {
                segment: &seg,
                id: id,
            };
            for &ref symdef in &seg.symbols {
                deviv(&mut self.symbols, &symdef.name, |slot| {
                    let address = TokenAddress::new3(id, symdef.start, symdef.ordinal);
                    slot_remove(&mut slot.all, address);
                    slot_remove(&mut slot.constant, address);
                });
            }

            for &ref labdef in &seg.labels {
                let label = sref.statement(labdef.index).label();
                deviv(&mut self.labels, label, |slot| {
                    slot_remove(slot, StatementAddress::new(id, labdef.index));
                });
            }

            for &ref floatdef in &seg.floats {
                deviv(&mut self.symbols, &floatdef.name, |slot| {
                    let address = StatementAddress::new(id, floatdef.start);
                    slot_remove(&mut slot.float, address);
                });
            }

            for &ref dvdef in &seg.global_dvs {
                slot_remove(&mut self.dv_info, StatementAddress::new(id, dvdef.start));
            }
        }
    }

    pub fn get_atom(&self, name: TokenPtr) -> Atom {
        self.atom_table.table.get(name).cloned().expect("please only use get_atom for local $v")
    }

    pub fn atom_name(&self, atom: Atom) -> TokenPtr {
        &self.atom_table.reverse[atom.0 as usize]
    }
}

pub struct NameReader<'a> {
    nameset: &'a Nameset,
}

pub struct LookupLabel {
    /// Address of topmost statement with this label
    pub address: StatementAddress,
}

pub struct LookupSymbol {
    pub stype: SymbolType,
    pub atom: Atom,
    /// Address of topmost global $c/$v with this token
    pub address: TokenAddress,
    /// Address of topmost global $c, if any
    pub const_address: Option<TokenAddress>,
}

pub struct LookupFloat<'a> {
    // again, topmost global float
    pub address: StatementAddress,
    pub label: TokenPtr<'a>,
    pub typecode: TokenPtr<'a>,
    pub typecode_atom: Atom,
}

pub struct LookupGlobalDv<'a> {
    pub address: StatementAddress,
    pub vars: &'a [Atom],
}

impl<'a> NameReader<'a> {
    pub fn new(nameset: &'a Nameset) -> Self {
        NameReader { nameset: nameset }
    }

    // TODO: add versions which fetch less data, to reduce dep tracking overhead
    pub fn lookup_label(&mut self, label: TokenPtr) -> Option<LookupLabel> {
        self.nameset
            .labels
            .get(label)
            .and_then(|&ref lslot| lslot.first().map(|&(addr, _)| LookupLabel { address: addr }))
    }

    pub fn lookup_symbol(&mut self, symbol: TokenPtr) -> Option<LookupSymbol> {
        self.nameset.symbols.get(symbol).and_then(|&ref syminfo| {
            syminfo.all.first().map(|&(addr, stype)| {
                LookupSymbol {
                    stype: stype,
                    atom: syminfo.atom,
                    address: addr,
                    const_address: syminfo.constant.first().map(|&(addr, _)| addr),
                }
            })
        })
    }

    // TODO: consider merging this with lookup_symbol
    pub fn lookup_float(&mut self, symbol: TokenPtr) -> Option<LookupFloat<'a>> {
        self.nameset.symbols.get(symbol).and_then(|&ref syminfo| {
            syminfo.float.first().map(|&(addr, (ref label, ref typecode, tcatom))| {
                LookupFloat {
                    address: addr,
                    label: &label,
                    typecode: &typecode,
                    typecode_atom: tcatom,
                }
            })
        })
    }

    pub fn lookup_global_dv(&mut self) -> Vec<LookupGlobalDv> {
        self.nameset
            .dv_info
            .iter()
            .map(|&(addr, ref vars)| {
                LookupGlobalDv {
                    address: addr,
                    vars: &vars,
                }
            })
            .collect()
    }
}
