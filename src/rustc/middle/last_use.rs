import syntax::{visit, ast_util};
import syntax::ast::*;
import syntax::codemap::span;
import std::list::{is_not_empty, list, nil, cons, tail};
import core::unreachable;
import std::list;
import std::map::hashmap;

// Last use analysis pass.
//
// Finds the last read of each value stored in a local variable or
// callee-owned argument (arguments with by-move or by-copy passing
// style). This is a limited form of liveness analysis, peformed
// (perhaps foolishly) directly on the AST.
//
// The algorithm walks the AST, keeping a set of (def, last_use)
// pairs. When the function is exited, or the local is overwritten,
// the current set of last uses is marked with 'true' in a table.
// Other branches may later overwrite them with 'false' again, since
// they may find a use coming after them. (Marking an expression as a
// last use is only done if it has not already been marked with
// 'false'.)
//
// Some complexity is added to deal with joining control flow branches
// (by `break` or conditionals), and for handling loops.

// Marks expr_paths that are last uses.
#[auto_serialize]
enum is_last_use {
    is_last_use,
    closes_over([node_id]),
}
type last_uses = std::map::hashmap<node_id, is_last_use>;
type spill_map = std::map::hashmap<node_id, ()>;

enum seen { unset, seen(node_id), }
enum block_type { func, lp, }

enum use { var_use(node_id), close_over(node_id), }
type set = [{def: node_id, uses: list<use>}];
type bl = @{type: block_type, mut second: bool, mut exits: [set]};

enum use_id { path(node_id), close(node_id, node_id) }
fn hash_use_id(id: use_id) -> uint {
    (alt id { path(i) { i } close(i, j) { (i << 10) + j } }) as uint
}

type ctx = {last_uses: std::map::hashmap<use_id, bool>,
            spill_map: std::map::hashmap<node_id, ()>,
            def_map: resolve::def_map,
            ref_map: alias::ref_map,
            tcx: ty::ctxt,
            // The current set of local last uses
            mut current: set,
            mut blocks: list<bl>};

fn find_last_uses(c: @crate, def_map: resolve::def_map,
                  ref_map: alias::ref_map, tcx: ty::ctxt)
    -> (last_uses, spill_map) {
    let v = visit::mk_vt(@{visit_expr: visit_expr,
                           visit_stmt: visit_stmt,
                           visit_fn: visit_fn
                           with *visit::default_visitor()});
    let cx = {last_uses: std::map::hashmap(hash_use_id, {|a, b| a == b}),
              spill_map: std::map::int_hash(),
              def_map: def_map,
              ref_map: ref_map,
              tcx: tcx,
              mut current: [],
              mut blocks: nil};
    visit::visit_crate(*c, cx, v);
    let mini_table = std::map::int_hash();
    cx.last_uses.items {|key, val|
        if val {
            alt key {
              path(id) {
                mini_table.insert(id, is_last_use);
                let def_node = ast_util::def_id_of_def(def_map.get(id)).node;
                cx.spill_map.insert(def_node, ());
              }
              close(fn_id, local_id) {
                cx.spill_map.insert(local_id, ());
                let known = alt check mini_table.find(fn_id) {
                  some(closes_over(ids)) { ids }
                  none { [] }
                };
                mini_table.insert(fn_id, closes_over(known + [local_id]));
              }
            }
        }
    }
    ret (mini_table, cx.spill_map);
}

fn visit_expr(ex: @expr, cx: ctx, v: visit::vt<ctx>) {
    alt ex.node {
      expr_ret(oexpr) {
        visit::visit_expr_opt(oexpr, cx, v);
        if !add_block_exit(cx, func) { leave_fn(cx); }
      }
      expr_fail(oexpr) {
        visit::visit_expr_opt(oexpr, cx, v);
        leave_fn(cx);
      }
      expr_break { add_block_exit(cx, lp); }
      expr_while(_, _) | expr_do_while(_, _) | expr_loop(_) {
        visit_block(lp, cx) {|| visit::visit_expr(ex, cx, v);}
      }
      expr_for(_, coll, blk) {
        v.visit_expr(coll, cx, v);
        visit_block(lp, cx) {|| visit::visit_block(blk, cx, v);}
      }
      expr_alt(input, arms, _) {
        v.visit_expr(input, cx, v);
        let before = cx.current;
        let mut sets = [];
        for arms.each {|arm|
            cx.current = before;
            v.visit_arm(arm, cx, v);
            sets += [cx.current];
        }
        cx.current = join_branches(sets);
      }
      expr_if(cond, then, els) {
        v.visit_expr(cond, cx, v);
        let mut cur = cx.current;
        visit::visit_block(then, cx, v);
        cx.current <-> cur;
        visit::visit_expr_opt(els, cx, v);
        cx.current = join_branches([cur, cx.current]);
      }
      expr_path(_) {
        let my_def = cx.def_map.get(ex.id);
        let my_def_id = ast_util::def_id_of_def(my_def).node;
        alt cx.ref_map.find(my_def_id) {
          option::some(root_id) {
            clear_in_current(cx, root_id, false);
          }
          _ {
            option::with_option_do(def_is_owned_local(cx, my_def)) {|nid|
                clear_in_current(cx, nid, false);
                cx.current += [{def: nid,
                                uses: cons(var_use(ex.id), @nil)}];
            }
          }
        }
      }
      expr_swap(lhs, rhs) {
        clear_if_path(cx, lhs, v, false);
        clear_if_path(cx, rhs, v, false);
      }
      expr_move(dest, src) | expr_assign(dest, src) {
        v.visit_expr(src, cx, v);
        clear_if_path(cx, dest, v, true);
      }
      expr_assign_op(_, dest, src) {
        v.visit_expr(src, cx, v);
        v.visit_expr(dest, cx, v);
        clear_if_path(cx, dest, v, true);
      }
      expr_fn(_, _, _, cap_clause) {
        // n.b.: safe to ignore copies, as if they are unused
        // then they are ignored, otherwise they will show up
        // as freevars in the body.
        vec::iter(cap_clause.moves) {|ci|
            clear_def_if_local(cx, cx.def_map.get(ci.id), false);
        }
        visit::visit_expr(ex, cx, v);
      }
      expr_call(f, args, _) {
        v.visit_expr(f, cx, v);
        let mut fns = [];
        let arg_ts = ty::ty_fn_args(ty::expr_ty(cx.tcx, f));
        vec::iter2(args, arg_ts) {|arg, arg_t|
            alt arg.node {
              expr_fn(_, _, _, _) | expr_fn_block(_, _)
              if is_blockish(ty::ty_fn_proto(arg_t.ty)) {
                fns += [arg];
              }
              _ {
                alt ty::arg_mode(cx.tcx, arg_t) {
                  by_mutbl_ref { clear_if_path(cx, arg, v, false); }
                  _ { v.visit_expr(arg, cx, v); }
                }
              }
            }
        }
        for fns.each {|f| v.visit_expr(f, cx, v); }
        vec::iter2(args, arg_ts) {|arg, arg_t|
            alt arg.node {
              expr_path(_) {
                alt ty::arg_mode(cx.tcx, arg_t) {
                  by_ref | by_val | by_mutbl_ref {
                    let def = cx.def_map.get(arg.id);
                    option::with_option_do(def_is_owned_local(cx, def)) {|id|
                        clear_in_current(cx, id, false);
                        cx.spill_map.insert(id, ());
                    }
                  }
                  _ {}
                }
              }
              _ {}
            }
        }
      }
      _ { visit::visit_expr(ex, cx, v); }
    }
}

fn visit_stmt(s: @stmt, cx: ctx, v: visit::vt<ctx>) {
    alt s.node {
      stmt_decl(@{node: decl_local(ls), _}, _) {
        shadow_in_current(cx, {|id|
            let mut rslt = false;
            for ls.each {|local|
                let mut found = false;
                pat_util::pat_bindings(cx.tcx.def_map, local.node.pat,
                                       {|pid, _a, _b|
                    if pid == id { found = true; }
                });
                if found { rslt = true; break; }
            }
            rslt
        });
      }
      _ {}
    }
    visit::visit_stmt(s, cx, v);
}

fn visit_fn(fk: visit::fn_kind, decl: fn_decl, body: blk,
            sp: span, id: node_id,
            cx: ctx, v: visit::vt<ctx>) {
    let fty = ty::node_id_to_type(cx.tcx, id);
    let proto = ty::ty_fn_proto(fty);
    alt proto {
      proto_any | proto_block {
        visit_block(func, cx, {||
            shadow_in_current(cx, {|id|
                vec::any(decl.inputs, {|arg| arg.id == id})
            });
            visit::visit_fn(fk, decl, body, sp, id, cx, v);
        });
      }
      proto_box | proto_uniq | proto_bare {
        alt cx.tcx.freevars.find(id) {
          some(vars) {
            for vec::each(*vars) {|v|
                option::with_option_do(def_is_owned_local(cx, v.def)) {|nid|
                    clear_in_current(cx, nid, false);
                    cx.current += [{def: nid,
                                    uses: cons(close_over(id), @nil)}];
                }
            }
          }
          _ {}
        }
        let mut old_cur = [], old_blocks = nil;
        cx.blocks <-> old_blocks;
        cx.current <-> old_cur;
        visit::visit_fn(fk, decl, body, sp, id, cx, v);
        cx.blocks <-> old_blocks;
        leave_fn(cx);
        cx.current <-> old_cur;
      }
    }
}

fn visit_block(tp: block_type, cx: ctx, visit: fn()) {
    let local = @{type: tp, mut second: false, mut exits: []};
    cx.blocks = cons(local, @cx.blocks);
    visit();
    local.second = true;
    local.exits = [];
    visit();
    let cx_blocks = cx.blocks;
    cx.blocks = tail(cx_blocks);
    local.exits += [cx.current];
    cx.current = join_branches(local.exits);
}

fn add_block_exit(cx: ctx, tp: block_type) -> bool {
    let mut cur = cx.blocks;
    while cur != nil {
        alt cur {
          cons(b, tail) {
            if (b.type == tp) {
                if !b.second { b.exits += [cx.current]; }
                ret true;
            }
            cur = *tail;
          }
          nil {
            // typestate can't use the while loop condition --
            // *sigh*
            unreachable();
          }
        }
    }
    ret false;
}

fn join_branches(branches: [set]) -> set {
    let mut found: set = [], i = 0u;
    let l = vec::len(branches);
    for branches.each {|set|
        i += 1u;
        for set.each {|elt|
            if !vec::any(found, {|v| v.def == elt.def}) {
                let mut j = i, nne = elt.uses;
                while j < l {
                    for vec::each(branches[j]) {|elt2|
                        if elt2.def == elt.def {
                            list::iter(elt2.uses) {|e|
                                if !list::has(nne, e) { nne = cons(e, @nne); }
                            }
                        }
                    }
                    j += 1u;
                }
                found += [{def: elt.def, uses: nne}];
            }
        }
    }
    ret found;
}

fn leave_fn(cx: ctx) {
    for cx.current.each {|elt|
        list::iter(elt.uses) {|use|
            let key = alt use {
              var_use(pth_id) { path(pth_id) }
              close_over(fn_id) { close(fn_id, elt.def) }
            };
            if !cx.last_uses.contains_key(key) {
                cx.last_uses.insert(key, true);
            }
        }
    }
}

fn shadow_in_current(cx: ctx, p: fn(node_id) -> bool) {
    let mut out = [];
    cx.current <-> out;
    for out.each {|e| if !p(e.def) { cx.current += [e]; } }
}

fn clear_in_current(cx: ctx, my_def: node_id, to: bool) {
    for cx.current.each {|elt|
        if elt.def == my_def {
            list::iter(elt.uses) {|use|
                let key = alt use {
                  var_use(pth_id) { path(pth_id) }
                  close_over(fn_id) { close(fn_id, elt.def) }
                };
                if !to || !cx.last_uses.contains_key(key) {
                    cx.last_uses.insert(key, to);
                }
            }
            cx.current = vec::filter(copy cx.current, {|x| x.def != my_def});
            break;
        }
    }
}

fn def_is_owned_local(cx: ctx, d: def) -> option<node_id> {
    alt d {
      def_local(id, _) { some(id) }
      def_arg(id, m) {
        alt ty::resolved_mode(cx.tcx, m) {
          by_copy | by_move { some(id) }
          by_ref | by_val | by_mutbl_ref { none }
        }
      }
      def_upvar(_, d, fn_id) {
        if is_blockish(ty::ty_fn_proto(ty::node_id_to_type(cx.tcx, fn_id))) {
            def_is_owned_local(cx, *d)
        } else { none }
      }
      _ { none }
    }
}

fn clear_def_if_local(cx: ctx, d: def, to: bool) {
    alt def_is_owned_local(cx, d) {
      some(nid) { clear_in_current(cx, nid, to); }
      _ {}
    }
}

fn clear_if_path(cx: ctx, ex: @expr, v: visit::vt<ctx>, to: bool) {
    alt ex.node {
      expr_path(_) { clear_def_if_local(cx, cx.def_map.get(ex.id), to); }
      _ { v.visit_expr(ex, cx, v); }
    }
}
