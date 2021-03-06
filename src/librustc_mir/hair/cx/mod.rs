// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

/*!
 * This module contains the code to convert from the wacky tcx data
 * structures into the hair. The `builder` is generally ignorant of
 * the tcx etc, and instead goes through the `Cx` for most of its
 * work.
 */

use hair::*;
use rustc::mir::repr::*;
use rustc::mir::transform::MirSource;

use rustc::middle::const_val::ConstVal;
use rustc_const_eval as const_eval;
use rustc_data_structures::indexed_vec::Idx;
use rustc::dep_graph::DepNode;
use rustc::hir::def_id::DefId;
use rustc::hir::intravisit::FnKind;
use rustc::hir::map::blocks::FnLikeNode;
use rustc::infer::InferCtxt;
use rustc::ty::subst::{Subst, Substs};
use rustc::ty::{self, Ty, TyCtxt};
use syntax::parse::token;
use rustc::hir;
use rustc_const_math::{ConstInt, ConstUsize};
use syntax::attr::AttrMetaMethods;

#[derive(Copy, Clone)]
pub struct Cx<'a, 'gcx: 'a+'tcx, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'gcx, 'tcx>,
    infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
    constness: hir::Constness,

    /// True if this constant/function needs overflow checks.
    check_overflow: bool
}

impl<'a, 'gcx, 'tcx> Cx<'a, 'gcx, 'tcx> {
    pub fn new(infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
               src: MirSource)
               -> Cx<'a, 'gcx, 'tcx> {
        let constness = match src {
            MirSource::Const(_) |
            MirSource::Static(..) => hir::Constness::Const,
            MirSource::Fn(id) => {
                let fn_like = FnLikeNode::from_node(infcx.tcx.map.get(id));
                match fn_like.map(|f| f.kind()) {
                    Some(FnKind::ItemFn(_, _, _, c, _, _, _)) => c,
                    Some(FnKind::Method(_, m, _, _)) => m.constness,
                    _ => hir::Constness::NotConst
                }
            }
            MirSource::Promoted(..) => bug!()
        };

        let src_node_id = src.item_id();

        // We are going to be accessing various tables
        // generated by TypeckItemBody; we also assume
        // that the body passes type check. These tables
        // are not individually tracked, so just register
        // a read here.
        let src_def_id = infcx.tcx.map.local_def_id(src_node_id);
        infcx.tcx.dep_graph.read(DepNode::TypeckItemBody(src_def_id));

        let attrs = infcx.tcx.map.attrs(src_node_id);

        // Some functions always have overflow checks enabled,
        // however, they may not get codegen'd, depending on
        // the settings for the crate they are translated in.
        let mut check_overflow = attrs.iter().any(|item| {
            item.check_name("rustc_inherit_overflow_checks")
        });

        // Respect -Z force-overflow-checks=on and -C debug-assertions.
        check_overflow |= infcx.tcx.sess.opts.debugging_opts.force_overflow_checks
               .unwrap_or(infcx.tcx.sess.opts.debug_assertions);

        // Constants and const fn's always need overflow checks.
        check_overflow |= constness == hir::Constness::Const;

        Cx {
            tcx: infcx.tcx,
            infcx: infcx,
            constness: constness,
            check_overflow: check_overflow
        }
    }
}

impl<'a, 'gcx, 'tcx> Cx<'a, 'gcx, 'tcx> {
    /// Normalizes `ast` into the appropriate `mirror` type.
    pub fn mirror<M: Mirror<'tcx>>(&mut self, ast: M) -> M::Output {
        ast.make_mirror(self)
    }

    pub fn usize_ty(&mut self) -> Ty<'tcx> {
        self.tcx.types.usize
    }

    pub fn usize_literal(&mut self, value: u64) -> Literal<'tcx> {
        match ConstUsize::new(value, self.tcx.sess.target.uint_type) {
            Ok(val) => Literal::Value { value: ConstVal::Integral(ConstInt::Usize(val))},
            Err(_) => bug!("usize literal out of range for target"),
        }
    }

    pub fn bool_ty(&mut self) -> Ty<'tcx> {
        self.tcx.types.bool
    }

    pub fn unit_ty(&mut self) -> Ty<'tcx> {
        self.tcx.mk_nil()
    }

    pub fn str_literal(&mut self, value: token::InternedString) -> Literal<'tcx> {
        Literal::Value { value: ConstVal::Str(value) }
    }

    pub fn true_literal(&mut self) -> Literal<'tcx> {
        Literal::Value { value: ConstVal::Bool(true) }
    }

    pub fn false_literal(&mut self) -> Literal<'tcx> {
        Literal::Value { value: ConstVal::Bool(false) }
    }

    pub fn const_eval_literal(&mut self, e: &hir::Expr) -> Literal<'tcx> {
        Literal::Value {
            value: const_eval::eval_const_expr(self.tcx.global_tcx(), e)
        }
    }

    pub fn trait_method(&mut self,
                        trait_def_id: DefId,
                        method_name: &str,
                        self_ty: Ty<'tcx>,
                        params: Vec<Ty<'tcx>>)
                        -> (Ty<'tcx>, Literal<'tcx>) {
        let method_name = token::intern(method_name);
        let substs = Substs::new_trait(params, vec![], self_ty);
        for trait_item in self.tcx.trait_items(trait_def_id).iter() {
            match *trait_item {
                ty::ImplOrTraitItem::MethodTraitItem(ref method) => {
                    if method.name == method_name {
                        let method_ty = self.tcx.lookup_item_type(method.def_id);
                        let method_ty = method_ty.ty.subst(self.tcx, &substs);
                        return (method_ty, Literal::Item {
                            def_id: method.def_id,
                            substs: self.tcx.mk_substs(substs),
                        });
                    }
                }
                ty::ImplOrTraitItem::ConstTraitItem(..) |
                ty::ImplOrTraitItem::TypeTraitItem(..) => {}
            }
        }

        bug!("found no method `{}` in `{:?}`", method_name, trait_def_id);
    }

    pub fn num_variants(&mut self, adt_def: ty::AdtDef) -> usize {
        adt_def.variants.len()
    }

    pub fn all_fields(&mut self, adt_def: ty::AdtDef, variant_index: usize) -> Vec<Field> {
        (0..adt_def.variants[variant_index].fields.len())
            .map(Field::new)
            .collect()
    }

    pub fn needs_drop(&mut self, ty: Ty<'tcx>) -> bool {
        let ty = self.tcx.lift_to_global(&ty).unwrap_or_else(|| {
            bug!("MIR: Cx::needs_drop({}) got \
                  type with inference types/regions", ty);
        });
        self.tcx.type_needs_drop_given_env(ty, &self.infcx.parameter_environment)
    }

    pub fn tcx(&self) -> TyCtxt<'a, 'gcx, 'tcx> {
        self.tcx
    }

    pub fn check_overflow(&self) -> bool {
        self.check_overflow
    }
}

mod block;
mod expr;
mod pattern;
mod to_ref;
