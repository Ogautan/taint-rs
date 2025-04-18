use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

use rustc_errors::struct_span_err;
use rustc_hir::def_id::DefId;
use rustc_index::bit_set::BitSet;
use rustc_middle::{
    mir::{
        traversal::reverse_postorder, visit::Visitor, BasicBlock, Body, Constant, HasLocalDecls,
        Local, Location, Operand, Place, Rvalue, Statement, StatementKind, Terminator,
        TerminatorKind,
    },
    ty::{TyCtxt, TyKind},
};

use rustc_mir_dataflow::{Analysis, AnalysisDomain, CallReturnPlaces, Forward};
use rustc_span::Span;

use tracing::instrument;

use crate::eval::attributes::{AttrInfo, AttrInfoKind};

use super::taint_domain::{PointsAwareTaintDomain, TaintDomain};

pub(crate) type PointsMap = HashMap<Local, HashSet<Local>>;
pub(crate) type Contexts = HashMap<(DefId, InitSet), Option<BitSet<Local>>>;

type InitSet = Vec<Option<bool>>;

/// A dataflow analysis that tracks whether a value may carry a taint.
///
/// Taints are introduced through sources, and consumed by sinks.
/// Ideally, a sink never consumes a tainted value - this should result in an error.
pub struct TaintAnalysis<'tcx, 'inter> {
    /// We use the type context to emit errors and get the MIR for other functions.
    tcx: TyCtxt<'tcx>,
    /// All the functions that have been marked
    info: &'inter AttrInfo,
    contexts: Rc<RefCell<Contexts>>,
    init: InitSet,
    points: RefCell<PointsMap>,
}

impl<'tcx, 'inter> TaintAnalysis<'tcx, 'inter> {
    /// Call on `main` function
    pub fn new(tcx: TyCtxt<'tcx>, info: &'inter AttrInfo) -> Self {
        Self::new_with_init(
            tcx,
            info,
            Rc::new(RefCell::new(Contexts::new())),
            InitSet::new(),
        )
    }

    /// Call on dependencies
    #[inline]
    fn new_with_init(
        tcx: TyCtxt<'tcx>,
        info: &'inter AttrInfo,
        contexts: Rc<RefCell<Contexts>>,
        init: InitSet,
    ) -> Self {
        TaintAnalysis {
            tcx,
            info,
            contexts,
            init,
            points: RefCell::new(PointsMap::new()),
        }
    }
}

struct TransferFunction<'tcx, 'inter, 'intra> {
    tcx: TyCtxt<'tcx>,
    info: &'inter AttrInfo,
    contexts: Rc<RefCell<Contexts>>,
    state: &'intra mut PointsAwareTaintDomain<'intra, Local>,
}

impl<'inter> AnalysisDomain<'inter> for TaintAnalysis<'_, '_> {
    type Domain = BitSet<Local>;
    const NAME: &'static str = "TaintAnalysis";

    type Direction = Forward;

    fn bottom_value(&self, body: &Body<'inter>) -> Self::Domain {
        // bottom = definitely untainted
        BitSet::new_empty(body.local_decls().len())
    }

    fn initialize_start_block(&self, body: &Body<'inter>, state: &mut Self::Domain) {
        // For the main function, locals all start out untainted.
        // For other functions, however, we must check if they receive tainted parameters.
        if !self.init.is_empty() {
            for (_, arg) in self
                .init
                .iter()
                .zip(body.args_iter())
                .filter(|(&t, _)| t.unwrap_or(false))
            {
                state.set_taint(arg, true);
            }
        }
    }
}

impl<'tcx, 'inter, 'intra> Analysis<'intra> for TaintAnalysis<'tcx, 'inter> {
    fn apply_statement_effect(
        &mut self,
        state: &mut Self::Domain,
        statement: &Statement<'intra>,
        location: Location,
    ) {
        TransferFunction {
            tcx: self.tcx,
            info: self.info,
            contexts: self.contexts.clone(),
            state: &mut PointsAwareTaintDomain {
                state,
                map: &mut self.points.borrow_mut(),
            },
        }
        .visit_statement(statement, location);
    }

    fn apply_terminator_effect(
        &mut self,
        state: &mut Self::Domain,
        terminator: &Terminator<'intra>,
        location: Location,
    ) {
        TransferFunction {
            tcx: self.tcx,
            info: self.info,
            contexts: self.contexts.clone(),
            state: &mut PointsAwareTaintDomain {
                state,
                map: &mut self.points.borrow_mut(),
            },
        }
        .visit_terminator(terminator, location);
    }

    fn apply_call_return_effect(
        &mut self,
        _state: &mut Self::Domain,
        _block: BasicBlock,
        _return_place: CallReturnPlaces<'_, 'intra>,
    ) {
        // do nothing
    }
}

impl std::fmt::Debug for TransferFunction<'_, '_, '_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{:?}", &self.state))
    }
}

impl<'inter> Visitor<'inter> for TransferFunction<'_, '_, '_> {
    fn visit_statement(&mut self, statement: &Statement<'inter>, _: Location) {
        let Statement { source_info, kind } = statement;

        self.visit_source_info(source_info);

        if let StatementKind::Assign(box (ref place, ref rvalue)) = kind {
            self.t_visit_assign(place, rvalue);
        }
    }

    fn visit_terminator(&mut self, terminator: &Terminator<'inter>, _: Location) {
        let Terminator { source_info, kind } = terminator;

        self.visit_source_info(source_info);

        match kind {
            TerminatorKind::Goto { .. } => {}
            TerminatorKind::SwitchInt { .. } => {}
            TerminatorKind::Return => {}
            TerminatorKind::Call {
                func: Operand::Constant(ref c),
                args,
                destination,
                fn_span,
                ..
            } => {
                self.t_visit_call(c, args, destination, fn_span);
            }
            TerminatorKind::Assert { .. } => {}
            _ => {}
        }
    }
}

impl<'long> TransferFunction<'_, '_, '_>
where
    Self: Visitor<'long>,
{
    #[instrument]
    fn t_visit_assign(&mut self, place: &Place, rvalue: &Rvalue) {
        match rvalue {
            // If we assign a constant to a place, the place is clean.
            Rvalue::Use(Operand::Constant(_)) | Rvalue::UnaryOp(_, Operand::Constant(_)) => {
                self.state.set_taint(place.local, false)
            }

            // Otherwise we propagate the taint
            Rvalue::Use(Operand::Copy(f) | Operand::Move(f)) => {
                self.state.propagate(f.local, place.local);
            }

            Rvalue::BinaryOp(_, box b) | Rvalue::CheckedBinaryOp(_, box b) => match b {
                (Operand::Constant(_), Operand::Constant(_)) => {
                    self.state.set_taint(place.local, false);
                }
                (Operand::Copy(a) | Operand::Move(a), Operand::Copy(b) | Operand::Move(b)) => {
                    if self.state.get_taint(a.local) || self.state.get_taint(b.local) {
                        self.state.set_taint(place.local, true);
                    } else {
                        self.state.set_taint(place.local, false);
                    }
                }
                (Operand::Copy(p) | Operand::Move(p), Operand::Constant(_))
                | (Operand::Constant(_), Operand::Copy(p) | Operand::Move(p)) => {
                    self.state.propagate(p.local, place.local);
                }
            },
            Rvalue::UnaryOp(_, Operand::Move(p) | Operand::Copy(p)) => {
                self.state.propagate(p.local, place.local);
            }
            Rvalue::Ref(_region_kind, _borrow_kind, p) => {
                self.state.add_ref(place, p);
            }

            Rvalue::Repeat(_, _) => {}
            Rvalue::ThreadLocalRef(_) => {}
            Rvalue::AddressOf(_, _) => {}
            Rvalue::Len(_) => {}
            Rvalue::Cast(_, _, _) => {}
            Rvalue::NullaryOp(_, _) => {}
            Rvalue::Discriminant(_) => {}
            Rvalue::Aggregate(_, _) => {}
            Rvalue::ShallowInitBox(_, _) | Rvalue::CopyForDeref(_) => {}
        }
    }

    #[instrument]
    fn t_visit_call(
        &mut self,
        func: &Constant,
        args: &[Operand],
        destination: &Place,
        span: &Span,
    ) {
        let name = func.to_string();
        let id = match func.literal.ty().kind() {
            TyKind::FnDef(id, _args) => Some(id),
            _ => None,
        }
        .unwrap();

        match self.info.get_kind(id) {
            Some(AttrInfoKind::Source) => self.t_visit_source_destination(destination),
            Some(AttrInfoKind::Sanitizer) => self.t_visit_sanitizer_destination(destination),
            Some(AttrInfoKind::Sink) => self.t_visit_sink(name, args, span),
            None => self.t_fn_call_analysis(args, id, destination),
        }
    }

    fn t_fn_call_analysis(
        &mut self,
        args: &[Operand],
        id: &rustc_hir::def_id::DefId,
        destination: &Place,
    ) {
        let init = args
            .iter()
            .map(|arg| match arg {
                Operand::Copy(p) | Operand::Move(p) => Some(self.state.get_taint(p.local)),
                Operand::Constant(_) => None,
            })
            .collect::<Vec<_>>();

        let end_state = self.t_function_summary(id, init);

        if let Some(end_state) = end_state {
            let return_place = Local::from_usize(0);

            if end_state.get_taint(return_place) {
                self.t_visit_source_destination(destination);
            }

            let target_body = self.tcx.optimized_mir(*id);
            let arg_map = args
                .iter()
                .map(|arg| arg.place().or(None))
                .zip(target_body.args_iter())
                .collect::<Vec<_>>();

            // Check if any variables which were passed in are tainted at this point.
            for (caller_arg, callee_arg) in arg_map {
                if let Some(place) = caller_arg {
                    self.state
                        .set_taint(place.local, end_state.get_taint(callee_arg));
                }
            }
        }
    }

    fn t_function_summary(&mut self, id: &DefId, init: Vec<Option<bool>>) -> Option<BitSet<Local>> {
        let key = (*id, init.clone());

        if let Some(summary) = self.t_get_cached_summary(&key) {
            summary
        } else {
            // In the case that we have recursive or mutually recursive function calls,
            // we make sure that we only compute a summary once per key by inserting None while we compute it.
            // For subsequent calls, calling `t_function_summary` will simply return None and the visitor will analyze other branches.
            self.t_insert_summary(&key, None);

            let target_body = self.tcx.optimized_mir(*id);
            let mut results =
                TaintAnalysis::new_with_init(self.tcx, self.info, self.contexts.clone(), init)
                    .into_engine(self.tcx, target_body)
                    .pass_name("taint_analysis")
                    .iterate_to_fixpoint()
                    .into_results_cursor(target_body);

            let state = if let Some((last, _)) = reverse_postorder(target_body).last() {
                results.seek_to_block_end(last);
                Some(results.get().clone())
            } else {
                None
            };

            // Once the function summary has been computed, we insert it into the cache.
            self.t_insert_summary(&key, state.clone());

            state
        }
    }

    fn t_insert_summary(&mut self, key: &(DefId, Vec<Option<bool>>), val: Option<BitSet<Local>>) {
        self.contexts.borrow_mut().insert(key.clone(), val);
    }

    fn t_get_cached_summary(
        &mut self,
        key: &(DefId, Vec<Option<bool>>),
    ) -> Option<Option<BitSet<Local>>> {
        let contexts = self.contexts.borrow();
        contexts.get(key).cloned()
    }

    fn t_visit_source_destination(&mut self, destination: &Place) {
        self.state.set_taint(destination.local, true);
    }

    fn t_visit_sanitizer_destination(&mut self, destination: &Place) {
        self.state.set_taint(destination.local, false);
    }

    fn t_visit_sink(&mut self, name: String, args: &[Operand], span: &Span) {
        if args.iter().map(|op| op.place()).any(|el| {
            if let Some(place) = el {
                self.state.get_taint(place.local)
            } else {
                false
            }
        }) {
            struct_span_err!(
                self.tcx.sess,
                *span,
                T0001,
                "function `{}` received tainted input",
                name
            )
            .emit();
        }
    }
}
