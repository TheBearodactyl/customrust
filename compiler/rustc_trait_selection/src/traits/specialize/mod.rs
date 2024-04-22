//! Logic and data structures related to impl specialization, explained in
//! greater detail below.
//!
//! At the moment, this implementation support only the simple "chain" rule:
//! If any two impls overlap, one must be a strict subset of the other.
//!
//! See the [rustc dev guide] for a bit more detail on how specialization
//! fits together with the rest of the trait machinery.
//!
//! [rustc dev guide]: https://rustc-dev-guide.rust-lang.org/traits/specialization.html

pub mod specialization_graph;
use rustc_infer::infer::DefineOpaqueTypes;
use specialization_graph::GraphExt;

use crate::errors::NegativePositiveConflict;
use crate::infer::{InferCtxt, InferOk, TyCtxtInferExt};
use crate::traits::select::IntercrateAmbiguityCause;
use crate::traits::{
    self, coherence, FutureCompatOverlapErrorKind, ObligationCause, ObligationCtxt,
};
use rustc_data_structures::fx::FxIndexSet;
use rustc_errors::{codes::*, DelayDm, Diag, EmissionGuarantee};
use rustc_hir::def_id::{DefId, LocalDefId};
use rustc_middle::ty::{self, ImplSubject, Ty, TyCtxt, TypeVisitableExt};
use rustc_middle::ty::{GenericArgs, GenericArgsRef};
use rustc_session::lint::builtin::COHERENCE_LEAK_CHECK;
use rustc_session::lint::builtin::ORDER_DEPENDENT_TRAIT_OBJECTS;
use rustc_span::{sym, ErrorGuaranteed, Span, DUMMY_SP};

use super::util;
use super::SelectionContext;

/// Information pertinent to an overlapping impl error.
#[derive(Debug)]
pub struct OverlapError<'tcx> {
    pub with_impl: DefId,
    pub trait_ref: ty::TraitRef<'tcx>,
    pub self_ty: Option<Ty<'tcx>>,
    pub intercrate_ambiguity_causes: FxIndexSet<IntercrateAmbiguityCause<'tcx>>,
    pub involves_placeholder: bool,
    pub overflowing_predicates: Vec<ty::Predicate<'tcx>>,
}

/// Given the generic parameters for the requested impl, translate it to the generic parameters
/// appropriate for the actual item definition (whether it be in that impl,
/// a parent impl, or the trait).
///
/// When we have selected one impl, but are actually using item definitions from
/// a parent impl providing a default, we need a way to translate between the
/// type parameters of the two impls. Here the `source_impl` is the one we've
/// selected, and `source_args` is its generic parameters.
/// And `target_node` is the impl/trait we're actually going to get the
/// definition from. The resulting instantiation will map from `target_node`'s
/// generics to `source_impl`'s generics as instantiated by `source_args`.
///
/// For example, consider the following scenario:
///
/// ```ignore (illustrative)
/// trait Foo { ... }
/// impl<T, U> Foo for (T, U) { ... }  // target impl
/// impl<V> Foo for (V, V) { ... }     // source impl
/// ```
///
/// Suppose we have selected "source impl" with `V` instantiated with `u32`.
/// This function will produce an instantiation with `T` and `U` both mapping to `u32`.
///
/// where-clauses add some trickiness here, because they can be used to "define"
/// an argument indirectly:
///
/// ```ignore (illustrative)
/// impl<'a, I, T: 'a> Iterator for Cloned<I>
///    where I: Iterator<Item = &'a T>, T: Clone
/// ```
///
/// In a case like this, the instantiation for `T` is determined indirectly,
/// through associated type projection. We deal with such cases by using
/// *fulfillment* to relate the two impls, requiring that all projections are
/// resolved.
pub fn translate_args<'tcx>(
    infcx: &InferCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    source_impl: DefId,
    source_args: GenericArgsRef<'tcx>,
    target_node: specialization_graph::Node,
) -> GenericArgsRef<'tcx> {
    translate_args_with_cause(infcx, param_env, source_impl, source_args, target_node, |_, _| {
        ObligationCause::dummy()
    })
}

/// Like [translate_args], but obligations from the parent implementation
/// are registered with the provided `ObligationCause`.
///
/// This is for reporting *region* errors from those bounds. Type errors should
/// not happen because the specialization graph already checks for those, and
/// will result in an ICE.
pub fn translate_args_with_cause<'tcx>(
    infcx: &InferCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    source_impl: DefId,
    source_args: GenericArgsRef<'tcx>,
    target_node: specialization_graph::Node,
    cause: impl Fn(usize, Span) -> ObligationCause<'tcx>,
) -> GenericArgsRef<'tcx> {
    debug!(
        "translate_args({:?}, {:?}, {:?}, {:?})",
        param_env, source_impl, source_args, target_node
    );
    let source_trait_ref =
        infcx.tcx.impl_trait_ref(source_impl).unwrap().instantiate(infcx.tcx, source_args);

    // translate the Self and Param parts of the generic parameters, since those
    // vary across impls
    let target_args = match target_node {
        specialization_graph::Node::Impl(target_impl) => {
            // no need to translate if we're targeting the impl we started with
            if source_impl == target_impl {
                return source_args;
            }

            fulfill_implication(infcx, param_env, source_trait_ref, source_impl, target_impl, cause)
                .unwrap_or_else(|()| {
                    bug!(
                        "When translating generic parameters from {source_impl:?} to \
                        {target_impl:?}, the expected specialization failed to hold"
                    )
                })
        }
        specialization_graph::Node::Trait(..) => source_trait_ref.args,
    };

    // directly inherent the method generics, since those do not vary across impls
    source_args.rebase_onto(infcx.tcx, source_impl, target_args)
}

/// Is `impl1` a specialization of `impl2`?
///
/// Specialization is determined by the sets of types to which the impls apply;
/// `impl1` specializes `impl2` if it applies to a subset of the types `impl2` applies
/// to.
#[instrument(skip(tcx), level = "debug")]
pub(super) fn specializes(tcx: TyCtxt<'_>, (impl1_def_id, impl2_def_id): (DefId, DefId)) -> bool {
    // The feature gate should prevent introducing new specializations, but not
    // taking advantage of upstream ones.
    // If specialization is enabled for this crate then no extra checks are needed.
    // If it's not, and either of the `impl`s is local to this crate, then this definitely
    // isn't specializing - unless specialization is enabled for the `impl` span,
    // e.g. if it comes from an `allow_internal_unstable` macro
    let features = tcx.features();
    let specialization_enabled = features.specialization || features.min_specialization;
    if !specialization_enabled {
        if impl1_def_id.is_local() {
            let span = tcx.def_span(impl1_def_id);
            if !span.allows_unstable(sym::specialization)
                && !span.allows_unstable(sym::min_specialization)
            {
                return false;
            }
        }

        if impl2_def_id.is_local() {
            let span = tcx.def_span(impl2_def_id);
            if !span.allows_unstable(sym::specialization)
                && !span.allows_unstable(sym::min_specialization)
            {
                return false;
            }
        }
    }

    let impl1_trait_header = tcx.impl_trait_header(impl1_def_id).unwrap();

    // We determine whether there's a subset relationship by:
    //
    // - replacing bound vars with placeholders in impl1,
    // - assuming the where clauses for impl1,
    // - instantiating impl2 with fresh inference variables,
    // - unifying,
    // - attempting to prove the where clauses for impl2
    //
    // The last three steps are encapsulated in `fulfill_implication`.
    //
    // See RFC 1210 for more details and justification.

    // Currently we do not allow e.g., a negative impl to specialize a positive one
    if impl1_trait_header.polarity != tcx.impl_polarity(impl2_def_id) {
        return false;
    }

    // create a parameter environment corresponding to a (placeholder) instantiation of impl1
    let penv = tcx.param_env(impl1_def_id);

    // Create an infcx, taking the predicates of impl1 as assumptions:
    let infcx = tcx.infer_ctxt().build();

    // Attempt to prove that impl2 applies, given all of the above.
    fulfill_implication(
        &infcx,
        penv,
        impl1_trait_header.trait_ref.instantiate_identity(),
        impl1_def_id,
        impl2_def_id,
        |_, _| ObligationCause::dummy(),
    )
    .is_ok()
}

/// Attempt to fulfill all obligations of `target_impl` after unification with
/// `source_trait_ref`. If successful, returns the generic parameters for *all* the
/// generics of `target_impl`, including both those needed to unify with
/// `source_trait_ref` and those whose identity is determined via a where
/// clause in the impl.
fn fulfill_implication<'tcx>(
    infcx: &InferCtxt<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
    source_trait_ref: ty::TraitRef<'tcx>,
    source_impl: DefId,
    target_impl: DefId,
    error_cause: impl Fn(usize, Span) -> ObligationCause<'tcx>,
) -> Result<GenericArgsRef<'tcx>, ()> {
    debug!(
        "fulfill_implication({:?}, trait_ref={:?} |- {:?} applies)",
        param_env, source_trait_ref, target_impl
    );

    let source_trait_ref =
        match traits::fully_normalize(infcx, ObligationCause::dummy(), param_env, source_trait_ref)
        {
            Ok(source_trait_ref) => source_trait_ref,
            Err(_errors) => {
                infcx.dcx().span_delayed_bug(
                    infcx.tcx.def_span(source_impl),
                    format!("failed to fully normalize {source_trait_ref}"),
                );
                source_trait_ref
            }
        };

    let source_trait = ImplSubject::Trait(source_trait_ref);

    let selcx = SelectionContext::new(infcx);
    let target_args = infcx.fresh_args_for_item(DUMMY_SP, target_impl);
    let (target_trait, obligations) =
        util::impl_subject_and_oblig(&selcx, param_env, target_impl, target_args, error_cause);

    // do the impls unify? If not, no specialization.
    let Ok(InferOk { obligations: more_obligations, .. }) = infcx
        .at(&ObligationCause::dummy(), param_env)
        // Ok to use `Yes`, as all the generic params are already replaced by inference variables,
        // which will match the opaque type no matter if it is defining or not.
        // Any concrete type that would match the opaque would already be handled by coherence rules,
        // and thus either be ok to match here and already have errored, or it won't match, in which
        // case there is no issue anyway.
        .eq(DefineOpaqueTypes::Yes, source_trait, target_trait)
    else {
        debug!("fulfill_implication: {:?} does not unify with {:?}", source_trait, target_trait);
        return Err(());
    };

    // Needs to be `in_snapshot` because this function is used to rebase
    // generic parameters, which may happen inside of a select within a probe.
    let ocx = ObligationCtxt::new(infcx);
    // attempt to prove all of the predicates for impl2 given those for impl1
    // (which are packed up in penv)
    ocx.register_obligations(obligations.chain(more_obligations));

    let errors = ocx.select_all_or_error();
    if !errors.is_empty() {
        // no dice!
        debug!(
            "fulfill_implication: for impls on {:?} and {:?}, \
                 could not fulfill: {:?} given {:?}",
            source_trait,
            target_trait,
            errors,
            param_env.caller_bounds()
        );
        return Err(());
    }

    debug!("fulfill_implication: an impl for {:?} specializes {:?}", source_trait, target_trait);

    // Now resolve the *generic parameters* we built for the target earlier, replacing
    // the inference variables inside with whatever we got from fulfillment.
    Ok(infcx.resolve_vars_if_possible(target_args))
}

/// Query provider for `specialization_graph_of`.
pub(super) fn specialization_graph_provider(
    tcx: TyCtxt<'_>,
    trait_id: DefId,
) -> Result<&'_ specialization_graph::Graph, ErrorGuaranteed> {
    let mut sg = specialization_graph::Graph::new();
    let overlap_mode = specialization_graph::OverlapMode::get(tcx, trait_id);

    let mut trait_impls: Vec<_> = tcx.all_impls(trait_id).collect();

    // The coherence checking implementation seems to rely on impls being
    // iterated over (roughly) in definition order, so we are sorting by
    // negated `CrateNum` (so remote definitions are visited first) and then
    // by a flattened version of the `DefIndex`.
    trait_impls
        .sort_unstable_by_key(|def_id| (-(def_id.krate.as_u32() as i64), def_id.index.index()));

    let mut errored = Ok(());

    for impl_def_id in trait_impls {
        if let Some(impl_def_id) = impl_def_id.as_local() {
            // This is where impl overlap checking happens:
            let insert_result = sg.insert(tcx, impl_def_id.to_def_id(), overlap_mode);
            // Report error if there was one.
            let (overlap, used_to_be_allowed) = match insert_result {
                Err(overlap) => (Some(overlap), None),
                Ok(Some(overlap)) => (Some(overlap.error), Some(overlap.kind)),
                Ok(None) => (None, None),
            };

            if let Some(overlap) = overlap {
                errored = errored.and(report_overlap_conflict(
                    tcx,
                    overlap,
                    impl_def_id,
                    used_to_be_allowed,
                ));
            }
        } else {
            let parent = tcx.impl_parent(impl_def_id).unwrap_or(trait_id);
            sg.record_impl_from_cstore(tcx, parent, impl_def_id)
        }
    }
    errored?;

    Ok(tcx.arena.alloc(sg))
}

// This function is only used when
// encountering errors and inlining
// it negatively impacts perf.
#[cold]
#[inline(never)]
fn report_overlap_conflict<'tcx>(
    tcx: TyCtxt<'tcx>,
    overlap: OverlapError<'tcx>,
    impl_def_id: LocalDefId,
    used_to_be_allowed: Option<FutureCompatOverlapErrorKind>,
) -> Result<(), ErrorGuaranteed> {
    let impl_polarity = tcx.impl_polarity(impl_def_id.to_def_id());
    let other_polarity = tcx.impl_polarity(overlap.with_impl);
    match (impl_polarity, other_polarity) {
        (ty::ImplPolarity::Negative, ty::ImplPolarity::Positive) => {
            Err(report_negative_positive_conflict(
                tcx,
                &overlap,
                impl_def_id,
                impl_def_id.to_def_id(),
                overlap.with_impl,
            ))
        }

        (ty::ImplPolarity::Positive, ty::ImplPolarity::Negative) => {
            Err(report_negative_positive_conflict(
                tcx,
                &overlap,
                impl_def_id,
                overlap.with_impl,
                impl_def_id.to_def_id(),
            ))
        }

        _ => report_conflicting_impls(tcx, overlap, impl_def_id, used_to_be_allowed),
    }
}

fn report_negative_positive_conflict<'tcx>(
    tcx: TyCtxt<'tcx>,
    overlap: &OverlapError<'tcx>,
    local_impl_def_id: LocalDefId,
    negative_impl_def_id: DefId,
    positive_impl_def_id: DefId,
) -> ErrorGuaranteed {
    tcx.dcx()
        .create_err(NegativePositiveConflict {
            impl_span: tcx.def_span(local_impl_def_id),
            trait_desc: overlap.trait_ref,
            self_ty: overlap.self_ty,
            negative_impl_span: tcx.span_of_impl(negative_impl_def_id),
            positive_impl_span: tcx.span_of_impl(positive_impl_def_id),
        })
        .emit()
}

fn report_conflicting_impls<'tcx>(
    tcx: TyCtxt<'tcx>,
    overlap: OverlapError<'tcx>,
    impl_def_id: LocalDefId,
    used_to_be_allowed: Option<FutureCompatOverlapErrorKind>,
) -> Result<(), ErrorGuaranteed> {
    let impl_span = tcx.def_span(impl_def_id);

    // Work to be done after we've built the Diag. We have to define it now
    // because the lint emit methods don't return back the Diag that's passed
    // in.
    fn decorate<'tcx, G: EmissionGuarantee>(
        tcx: TyCtxt<'tcx>,
        overlap: &OverlapError<'tcx>,
        impl_span: Span,
        err: &mut Diag<'_, G>,
    ) {
        match tcx.span_of_impl(overlap.with_impl) {
            Ok(span) => {
                err.span_label(span, "first implementation here");

                err.span_label(
                    impl_span,
                    format!(
                        "conflicting implementation{}",
                        overlap.self_ty.map_or_else(String::new, |ty| format!(" for `{ty}`"))
                    ),
                );
            }
            Err(cname) => {
                let msg = match to_pretty_impl_header(tcx, overlap.with_impl) {
                    Some(s) => {
                        format!("conflicting implementation in crate `{cname}`:\n- {s}")
                    }
                    None => format!("conflicting implementation in crate `{cname}`"),
                };
                err.note(msg);
            }
        }

        for cause in &overlap.intercrate_ambiguity_causes {
            cause.add_intercrate_ambiguity_hint(err);
        }

        if overlap.involves_placeholder {
            coherence::add_placeholder_note(err);
        }

        if !overlap.overflowing_predicates.is_empty() {
            coherence::suggest_increasing_recursion_limit(
                tcx,
                err,
                &overlap.overflowing_predicates,
            );
        }
    }

    let msg = DelayDm(|| {
        format!(
            "conflicting implementations of trait `{}`{}{}",
            overlap.trait_ref.print_trait_sugared(),
            overlap.self_ty.map_or_else(String::new, |ty| format!(" for type `{ty}`")),
            match used_to_be_allowed {
                Some(FutureCompatOverlapErrorKind::Issue33140) => ": (E0119)",
                _ => "",
            }
        )
    });

    // Don't report overlap errors if the header references error
    if let Err(err) = (overlap.trait_ref, overlap.self_ty).error_reported() {
        return Err(err);
    }

    match used_to_be_allowed {
        None => {
            let reported = if overlap.with_impl.is_local()
                || tcx.ensure().orphan_check_impl(impl_def_id).is_ok()
            {
                let mut err = tcx.dcx().struct_span_err(impl_span, msg);
                err.code(E0119);
                decorate(tcx, &overlap, impl_span, &mut err);
                err.emit()
            } else {
                tcx.dcx().span_delayed_bug(impl_span, "impl should have failed the orphan check")
            };
            Err(reported)
        }
        Some(kind) => {
            let lint = match kind {
                FutureCompatOverlapErrorKind::Issue33140 => ORDER_DEPENDENT_TRAIT_OBJECTS,
                FutureCompatOverlapErrorKind::LeakCheck => COHERENCE_LEAK_CHECK,
            };
            tcx.node_span_lint(
                lint,
                tcx.local_def_id_to_hir_id(impl_def_id),
                impl_span,
                msg,
                |err| {
                    decorate(tcx, &overlap, impl_span, err);
                },
            );
            Ok(())
        }
    }
}

/// Recovers the "impl X for Y" signature from `impl_def_id` and returns it as a
/// string.
pub(crate) fn to_pretty_impl_header(tcx: TyCtxt<'_>, impl_def_id: DefId) -> Option<String> {
    use std::fmt::Write;

    let trait_ref = tcx.impl_trait_ref(impl_def_id)?.instantiate_identity();
    let mut w = "impl".to_owned();

    let args = GenericArgs::identity_for_item(tcx, impl_def_id);

    // FIXME: Currently only handles ?Sized.
    //        Needs to support ?Move and ?DynSized when they are implemented.
    let mut types_without_default_bounds = FxIndexSet::default();
    let sized_trait = tcx.lang_items().sized_trait();

    let arg_names = args.iter().map(|k| k.to_string()).filter(|k| k != "'_").collect::<Vec<_>>();
    if !arg_names.is_empty() {
        types_without_default_bounds.extend(args.types());
        w.push('<');
        w.push_str(&arg_names.join(", "));
        w.push('>');
    }

    write!(
        w,
        " {} for {}",
        trait_ref.print_only_trait_path(),
        tcx.type_of(impl_def_id).instantiate_identity()
    )
    .unwrap();

    // The predicates will contain default bounds like `T: Sized`. We need to
    // remove these bounds, and add `T: ?Sized` to any untouched type parameters.
    let predicates = tcx.predicates_of(impl_def_id).predicates;
    let mut pretty_predicates =
        Vec::with_capacity(predicates.len() + types_without_default_bounds.len());

    for (p, _) in predicates {
        if let Some(poly_trait_ref) = p.as_trait_clause() {
            if Some(poly_trait_ref.def_id()) == sized_trait {
                // FIXME(#120456) - is `swap_remove` correct?
                types_without_default_bounds.swap_remove(&poly_trait_ref.self_ty().skip_binder());
                continue;
            }
        }
        pretty_predicates.push(p.to_string());
    }

    pretty_predicates.extend(types_without_default_bounds.iter().map(|ty| format!("{ty}: ?Sized")));

    if !pretty_predicates.is_empty() {
        write!(w, "\n  where {}", pretty_predicates.join(", ")).unwrap();
    }

    w.push(';');
    Some(w)
}
