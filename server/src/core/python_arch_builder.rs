use std::rc::Rc;
use std::cell::RefCell;
use std::vec;
use anyhow::Error;
use ruff_text_size::TextRange;
use ruff_python_ast::{Alias, Expr, Identifier, Stmt, StmtAnnAssign, StmtAssign, StmtClassDef, StmtFor, StmtFunctionDef, StmtIf, StmtTry};
use lsp_types::Diagnostic;
use tracing::{info, warn};
use weak_table::traits::WeakElement;
use weak_table::PtrWeakHashSet;
use std::path::PathBuf;

use crate::constants::{BuildStatus, BuildSteps, SymType};
use crate::core::python_utils;
use crate::core::import_resolver::resolve_import_stmt;
use crate::core::symbols::symbol::Symbol;
use crate::core::evaluation::{Evaluation, EvaluationValue};
use crate::core::python_arch_builder_hooks::PythonArchBuilderHooks;
use crate::threads::SessionInfo;
use crate::utils::PathSanitizer as _;
use crate::S;

use super::import_resolver::ImportResult;
use super::symbols::class_symbol::ClassSymbol;
use super::symbols::function_symbol::{Argument, FunctionSymbol};


#[derive(Debug)]
pub struct PythonArchBuilder {
    sym_stack: Vec<Rc<RefCell<Symbol>>>,
    __all_symbols_to_add: Vec<(String, TextRange)>,
    diagnostics: Vec<Diagnostic>
}

impl PythonArchBuilder {
    pub fn new(symbol: Rc<RefCell<Symbol>>) -> PythonArchBuilder {
        PythonArchBuilder {
            sym_stack: vec![symbol],
            __all_symbols_to_add: Vec::new(),
            diagnostics: vec![]
        }
    }

    pub fn load_arch(&mut self, session: &mut SessionInfo) -> Result<(), Error> {
        //println!("load arch");
        let mut symbol = self.sym_stack[0].borrow_mut();
        if [SymType::NAMESPACE, SymType::ROOT, SymType::COMPILED, SymType::VARIABLE].contains(&symbol.typ()) {
            return Ok(()); // nothing to extract
        }
        symbol.set_build_status(BuildSteps::ARCH, BuildStatus::IN_PROGRESS);
        if symbol.paths().len() != 1 {
            panic!()
        }
        let mut path = symbol.paths()[0].clone();
        //println!("load arch path: {}", path);
        if symbol.typ() == SymType::PACKAGE {
            path = PathBuf::from(path).join("__init__.py").sanitize() + symbol.as_package().i_ext().as_str();
        }
        let in_workspace = (symbol.parent().is_some() &&
            symbol.parent().as_ref().unwrap().upgrade().is_some() &&
            symbol.parent().as_ref().unwrap().upgrade().unwrap().borrow().in_workspace()) ||
            session.sync_odoo.get_file_mgr().borrow().is_in_workspace(path.as_str());
        symbol.set_in_workspace(in_workspace);
        drop(symbol);
        let file_info = session.sync_odoo.get_file_mgr().borrow_mut().update_file_info(session, path.as_str(), None, None, false); //create ast if not in cache
        let mut file_info = (*file_info).borrow_mut();
        file_info.replace_diagnostics(BuildSteps::ARCH, self.diagnostics.clone());
        if file_info.ast.is_some() {
            self.visit_node(session, &file_info.ast.as_ref().unwrap())?;
            self._resolve_all_symbols(session);
            session.sync_odoo.add_to_rebuild_arch_eval(self.sym_stack[0].clone());
        } else {
            file_info.publish_diagnostics(session);
        }
        PythonArchBuilderHooks::on_done(session, &self.sym_stack[0]);
        let mut symbol = self.sym_stack[0].borrow_mut();
        symbol.set_build_status(BuildSteps::ARCH, BuildStatus::DONE);
        Ok(())
    }

    fn create_local_symbols_from_import_stmt(&mut self, session: &mut SessionInfo, from_stmt: Option<&Identifier>, name_aliases: &[Alias], level: Option<u32>, range: &TextRange) -> Result<(), Error> {
        for import_name in name_aliases {
            if import_name.name.as_str() == "*" {
                if self.sym_stack.len() != 1 { //only at top level for now.
                    continue;
                }
                let import_result: ImportResult = resolve_import_stmt(
                    session,
                    self.sym_stack.last().unwrap(),
                    from_stmt,
                    name_aliases,
                    level,
                    range).remove(0); //we don't need the vector with this call as there will be 1 result.
                if !import_result.found {
                    session.sync_odoo.not_found_symbols.insert(self.sym_stack[0].clone());
                    let file_tree_flattened = vec![import_result.file_tree.0.clone(), import_result.file_tree.1.clone()].concat();
                    self.sym_stack[0].borrow_mut().not_found_paths_mut().push((BuildSteps::ARCH, file_tree_flattened));
                    continue;
                }
                let mut all_name_allowed = true;
                let mut name_filter: Vec<String> = vec![];
                if let Some(all) = import_result.symbol.borrow().get_content_symbol("__all__", u32::MAX).get(0) {
                    let all = Symbol::follow_ref(all, session, &mut None, false, true, &mut self.diagnostics);
                    if let Some(all) = all.get(0) {
                        if !all.0.is_expired() {
                            let all = all.0.upgrade();
                            if let Some(all) = all {
                                let all = (*all).borrow();
                                if all.evaluations().is_some() && all.evaluations().unwrap().len() == 1 {
                                    let value = &all.evaluations().unwrap()[0].value;
                                    if value.is_some() {
                                        let (nf, parse_error) = self.extract_all_symbol_eval_values(&value.as_ref());
                                        if parse_error {
                                            warn!("error during parsing __all__ import in file {}", (*import_result.symbol).borrow().paths()[0] )
                                        }
                                        name_filter = nf;
                                        all_name_allowed = false;
                                    } else {
                                        warn!("invalid __all__ import in file {} - no value found", (*import_result.symbol).borrow().paths()[0])
                                    }
                                } else {
                                    warn!("invalid __all__ import in file {} - multiple evaluation found", (*import_result.symbol).borrow().paths()[0])
                                }
                            } else {
                                warn!("invalid __all__ import in file {} - localizedSymbol not found", (*import_result.symbol).borrow().paths()[0])
                            }
                        } else {
                            warn!("invalid __all__ import in file {} - expired symbol", (*import_result.symbol).borrow().paths()[0])
                        }
                    } else {
                        warn!("invalid __all__ import in file {} - no symbol found", (*import_result.symbol).borrow().paths()[0])
                    }
                }
                let mut dep_to_add = vec![];
                let symbol = import_result.symbol.borrow();
                for (name, loc_syms) in symbol.iter_symbols() {
                    if all_name_allowed || name_filter.contains(&name) {
                        let mut variable = self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &name, &import_result.range);
                        let mut loc = variable.borrow_mut();
                        loc.as_variable_mut().is_import_variable = true;
                        loc.as_variable_mut().evaluations = Evaluation::from_sections(&symbol, loc_syms);
                        dep_to_add.push(variable.clone());
                    }
                }
                drop(symbol);
                for sym in dep_to_add {
                    let mut sym_bw = sym.borrow_mut();
                    let evaluation = &sym_bw.as_variable_mut().evaluations[0];
                    let evaluated_type = &evaluation.symbol;
                    let evaluated_type = evaluated_type.get_symbol(session, &mut None, &mut self.diagnostics).0;
                    if !evaluated_type.is_expired() {
                        let evaluated_type = evaluated_type.upgrade().unwrap();
                        let evaluated_type_file = evaluated_type.borrow_mut().get_file().unwrap().clone().upgrade().unwrap();
                        if !Rc::ptr_eq(&self.sym_stack[0], &evaluated_type_file) {
                            self.sym_stack[0].borrow_mut().add_dependency(&mut evaluated_type_file.borrow_mut(), BuildSteps::ARCH, BuildSteps::ARCH);
                        }
                    }
                }

            } else {
                let var_name = if import_name.asname.is_none() {
                    S!(import_name.name.split(".").next().unwrap())
                } else {
                    import_name.asname.as_ref().unwrap().clone().to_string()
                };
                let mut variable = self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &var_name, &import_name.range);
                variable.borrow_mut().as_variable_mut().is_import_variable = true;
            }
        }
        Ok(())
    }

    fn visit_node(&mut self, session: &mut SessionInfo, nodes: &Vec<Stmt>) -> Result<(), Error> {
        for stmt in nodes.iter() {
            match stmt {
                Stmt::Import(import_stmt) => {
                    if self.sym_stack.last().unwrap().borrow().typ() != SymType::FUNCTION {
                        self.create_local_symbols_from_import_stmt(session, None, &import_stmt.names, None, &import_stmt.range)?
                    }
                },
                Stmt::ImportFrom(import_from_stmt) => {
                    if self.sym_stack.last().unwrap().borrow().typ() != SymType::FUNCTION {
                        self.create_local_symbols_from_import_stmt(session, import_from_stmt.module.as_ref(), &import_from_stmt.names, Some(import_from_stmt.level), &import_from_stmt.range)?
                    }
                },
                Stmt::AnnAssign(ann_assign_stmt) => {
                    if self.sym_stack.last().unwrap().borrow().typ() != SymType::FUNCTION {
                        self._visit_ann_assign(session, ann_assign_stmt);
                    }
                },
                Stmt::Assign(assign_stmt) => {
                    if self.sym_stack.last().unwrap().borrow().typ() != SymType::FUNCTION {
                        self._visit_assign(session, assign_stmt);
                    }
                },
                Stmt::FunctionDef(function_def_stmt) => {
                    self.visit_func_def(session, function_def_stmt)?;
                },
                Stmt::ClassDef(class_def_stmt) => {
                    self.visit_class_def(session, class_def_stmt)?;
                },
                Stmt::If(if_stmt) => {
                    self.visit_if(session, if_stmt);
                },
                Stmt::Try(try_stmt) => {
                    self.visit_try(session, try_stmt);
                },
                Stmt::For(for_stmt) => {
                    self.visit_for(session, for_stmt);
                },
                _ => {}
            }
        }
        Ok(())
    }

    fn extract_all_symbol_eval_values(&self, value: &Option<&EvaluationValue>) -> (Vec<String>, bool) {
        let mut parse_error = false;
        let vec: Vec<String> = match value {
            Some(eval) => {
                match eval {
                    EvaluationValue::ANY() => {
                        parse_error = true;
                        vec![]
                    }
                    EvaluationValue::CONSTANT(c) => {
                        match c {
                            Expr::StringLiteral(s) => {
                                vec![s.value.to_string()]
                            },
                            _ => {parse_error = true; vec![]}
                        }
                    },
                    EvaluationValue::DICT(_d) => {
                        parse_error = true; vec![]
                    },
                    EvaluationValue::LIST(l) => {
                        let mut res = vec![];
                        for v in l.iter() {
                            match v {
                                Expr::StringLiteral(s) => {
                                    res.push(s.value.to_string());
                                }
                                _ => {parse_error = true; }
                            }
                        }
                        res
                    },
                    EvaluationValue::TUPLE(t) => {
                        let mut res = vec![];
                        for v in t.iter() {
                            match v {
                                Expr::StringLiteral(s) => {
                                    res.push(s.value.to_string());
                                }
                                _ => {parse_error = true; }
                            }
                        }
                        res
                    }
                }
            },
            None => {parse_error = true; vec![]}
        };
        (vec, parse_error)
    }

    fn _visit_ann_assign(&mut self, session: &mut SessionInfo, ann_assign_stmt: &StmtAnnAssign) {
        let assigns = match ann_assign_stmt.value.as_ref() {
            Some(value) => python_utils::unpack_assign(&vec![*ann_assign_stmt.target.clone()], Some(&ann_assign_stmt.annotation), Some(value)),
            None => python_utils::unpack_assign(&vec![*ann_assign_stmt.target.clone()], Some(&ann_assign_stmt.annotation), None)
        };
        for assign in assigns.iter() { //should only be one
            let mut variable = self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &assign.target.id, &assign.target.range);
        }
    }

    fn _visit_assign(&mut self, session: &mut SessionInfo, assign_stmt: &StmtAssign) {
        let assigns = python_utils::unpack_assign(&assign_stmt.targets, None, Some(&assign_stmt.value));
        for assign in assigns.iter() {
            let mut variable = self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &assign.target.id, &assign.target.range);
            let mut variable = variable.borrow_mut();
            if variable.name() == "__all__" && assign.value.is_some() && variable.parent().is_some() {
                let parent = variable.parent().as_ref().unwrap().upgrade();
                if parent.is_some() {
                    let parent = parent.unwrap();
                    let eval = Evaluation::eval_from_ast(session, &assign.value.as_ref().unwrap(), parent, &assign_stmt.range.start());
                    variable.as_variable_mut().evaluations = eval.0;
                    self.diagnostics.extend(eval.1);
                    if !variable.as_variable().evaluations.is_empty() {
                        if (*self.sym_stack.last().unwrap()).borrow().is_external() {
                            // external packages often import symbols from compiled files
                            // or with meta programmation like globals["var"] = __get_func().
                            // we don't want to handle that, so just declare __all__ content
                            // as symbols to not raise any error.
                            let evaluation = variable.as_variable_mut().evaluations.get(0).unwrap();
                            match &evaluation.value {
                                Some(EvaluationValue::LIST(list)) => {
                                    for item in list.iter() {
                                        match item {
                                            Expr::StringLiteral(s) => {
                                                self.__all_symbols_to_add.push((s.value.to_string(), evaluation.range.unwrap()));
                                            },
                                            _ => {}
                                        }
                                    }
                                },
                                _ => {}
                            }
                        }
                    }
                }
            }
        }
    }

    fn visit_func_def(&mut self, session: &mut SessionInfo, func_def: &StmtFunctionDef) -> Result<(), Error> {
        let mut sym = self.sym_stack.last().unwrap().borrow_mut().add_new_function(session, &func_def.name.id, &func_def.range);
        let mut sym_bw = sym.borrow_mut();
        let mut func_sym = sym_bw.as_func_mut();
        for decorator in func_def.decorator_list.iter() {
            if decorator.expression.is_name_expr() && decorator.expression.as_name_expr().unwrap().id.to_string() == "staticmethod" {
                func_sym.is_static = true;
            }
            if decorator.expression.is_name_expr() && decorator.expression.as_name_expr().unwrap().id.to_string() == "property" {
                func_sym.is_property = true;
            }
        }
        if func_def.body.len() > 0 && func_def.body[0].is_expr_stmt() {
            let expr: &ruff_python_ast::StmtExpr = func_def.body[0].as_expr_stmt().unwrap();
            if let Some(s) = expr.value.as_string_literal_expr() {
                func_sym.doc_string = Some(s.value.to_string())
            }
        }
        drop(sym_bw);
        //add params
        for arg in func_def.parameters.posonlyargs.iter().chain(&func_def.parameters.args) {
            let param = sym.borrow_mut().add_new_variable(session, &arg.parameter.name.id, &arg.range);
            param.borrow_mut().as_variable_mut().is_parameter = true;
            sym.borrow_mut().as_func_mut().args.push(Argument {
                symbol: Rc::downgrade(&param),
                default_value: None,
                is_args: false,
                is_kwargs: false,
            });
        }
        //visit body
        self.sym_stack.push(sym);
        self.visit_node(session, &func_def.body)?;
        self.sym_stack.pop();
        Ok(())
    }

    fn visit_class_def(&mut self, session: &mut SessionInfo, class_def: &StmtClassDef) -> Result<(), Error> {
        let mut sym = self.sym_stack.last().unwrap().borrow_mut().add_new_class(session, &class_def.name.id, &class_def.range);
        let mut sym_bw = sym.borrow_mut();
        let class_sym = sym_bw.as_class_sym_mut();
        if class_def.body.len() > 0 && class_def.body[0].is_expr_stmt() {
            let expr = class_def.body[0].as_expr_stmt().unwrap();
            if expr.value.is_literal_expr() {
                let const_expr = expr.value.as_literal_expr().unwrap();
                if let Some(s) = const_expr.as_string_literal() {
                    class_sym.doc_string = Some(s.value.to_string());
                }
            }
        }
        drop(sym_bw);
        self.sym_stack.push(sym.clone());
        self.visit_node(session, &class_def.body)?;
        self.sym_stack.pop();
        PythonArchBuilderHooks::on_class_def(session, sym);
        Ok(())
    }

    fn _resolve_all_symbols(&mut self, session: &mut SessionInfo) {
        for (symbol_name, range) in self.__all_symbols_to_add.drain(..) {
            if self.sym_stack.last().unwrap().borrow().get_content_symbol(&symbol_name, u32::MAX).is_empty() {
                let all_var = self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &symbol_name, &range);
            }
        }
    }

    fn visit_if(&mut self, session: &mut SessionInfo, if_stmt: &StmtIf) -> Result<(), Error> {
        //TODO check platform condition (sys.version > 3.12, etc...)
        self.visit_node(session, &if_stmt.body)?;
        for else_clause in if_stmt.elif_else_clauses.iter() {
            self.visit_node(session, &else_clause.body)?;
        }
        Ok(())
    }

    fn visit_for(&mut self, session: &mut SessionInfo, for_stmt: &StmtFor) -> Result<(), Error> {
        let unpacked = python_utils::unpack_assign(&vec![*for_stmt.target.clone()], None, None);
        for assign in unpacked {
            self.sym_stack.last().unwrap().borrow_mut().add_new_variable(session, &assign.target.id, &assign.target.range);
        }
        self.visit_node(session, &for_stmt.body)?;
        //TODO should split evaluations as in if
        self.visit_node(session, &for_stmt.orelse)?;
        Ok(())
    }

    fn visit_try(&mut self, session: &mut SessionInfo, try_stmt: &StmtTry) -> Result<(), Error> {
        self.visit_node(session, &try_stmt.body)?;
        self.visit_node(session, &try_stmt.orelse)?;
        self.visit_node(session, &try_stmt.finalbody)?;
        Ok(())
    }
}