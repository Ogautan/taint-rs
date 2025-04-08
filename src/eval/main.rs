use rustc_hir::def_id::DefId;
use rustc_middle::ty::TyCtxt;
use rustc_mir_dataflow::Analysis;

use crate::eval::attributes::TaintAttributeFinder;
use crate::taint_analysis::TaintAnalysis;

pub fn eval_main(tcx: TyCtxt<'_>, main_id: DefId) {
    // Find all functions in the current crate that have been tagged
    let mut finder = TaintAttributeFinder::new(tcx);
    tcx.hir().visit_all_item_likes_in_crate(&mut finder);

    let entry = tcx.optimized_mir(main_id);

    let _ = TaintAnalysis::new(tcx, &finder.info)
        .into_engine(tcx, entry)
        .pass_name("taint_analysis")
        .iterate_to_fixpoint();
}

pub fn eval_all_pub_fn(tcx: TyCtxt<'_>) {
    let mut finder = TaintAttributeFinder::new(tcx);
    tcx.hir().visit_all_item_likes_in_crate(&mut finder);
    for def_id in tcx
        .mir_keys(())
        .iter()
        .filter(|&&def_id| tcx.visibility(def_id).is_public())
    {
        let mir = tcx.optimized_mir(*def_id);
        let _ = TaintAnalysis::new(tcx, &finder.info)
            .into_engine(tcx, mir)
            .pass_name("taint_analysis")
            .iterate_to_fixpoint();
    }
}
