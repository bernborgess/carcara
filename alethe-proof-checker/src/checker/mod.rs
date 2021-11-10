mod rules;

use crate::{
    ast::*,
    benchmarking::{Metrics, StepId},
};
use ahash::{AHashMap, AHashSet};
use rules::{Rule, RuleArgs, RuleError};
use std::{
    fmt,
    time::{Duration, Instant},
};

#[derive(Debug)]
pub struct CheckerError {
    inner: RuleError,
    rule_name: String,
    step: String,
}

impl fmt::Display for CheckerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} (on step '{}', with rule '{}')",
            self.inner, self.step, self.rule_name
        )
    }
}

type CheckerResult = Result<(), CheckerError>;

struct Context {
    substitutions: AHashMap<Rc<Term>, Rc<Term>>,
    substitutions_until_fixed_point: AHashMap<Rc<Term>, Rc<Term>>,
    cumulative_substitutions: AHashMap<Rc<Term>, Rc<Term>>,
    bindings: AHashSet<SortedVar>,
}

#[derive(Debug)]
pub struct CheckerStatistics<'s> {
    pub file_name: &'s str,
    pub checking_time: &'s mut Duration,
    pub step_time: &'s mut Metrics<StepId>,
    pub step_time_by_file: &'s mut AHashMap<String, Metrics<StepId>>,
    pub step_time_by_rule: &'s mut AHashMap<String, Metrics<StepId>>,
}

#[derive(Debug, Default)]
pub struct Config<'c> {
    pub skip_unknown_rules: bool,
    pub is_running_test: bool,
    pub statistics: Option<CheckerStatistics<'c>>,
}

pub struct ProofChecker<'c> {
    pool: TermPool,
    config: Config<'c>,
    context: Vec<Context>,
}

impl<'c> ProofChecker<'c> {
    pub fn new(pool: TermPool, config: Config<'c>) -> Self {
        ProofChecker { pool, config, context: Vec::new() }
    }

    pub fn check(&mut self, proof: &Proof) -> CheckerResult {
        // Similarly to the parser, to avoid stack overflows in proofs with many nested subproofs,
        // we check the subproofs iteratively, instead of recursively

        // A stack of the subproof commands, and the index of the command being currently checked
        let mut commands_stack = vec![(0, proof.commands.as_slice())];

        while let Some(&(i, commands)) = commands_stack.last() {
            if i == commands.len() {
                // If we got to the end without popping the commands vector off the stack, we must
                // not be in a subproof
                assert!(commands_stack.len() == 1);
                break;
            }
            match &commands[i] {
                // The parser already ensures that the last command in a subproof is always a
                // "step" command
                ProofCommand::Step(step) if commands_stack.len() > 1 && i == commands.len() - 1 => {
                    self.check_step(step, &commands_stack, true)?;

                    // If this is the last command of a subproof, we have to pop the subproof
                    // commands off of the stack
                    commands_stack.pop();
                    self.context.pop();
                    Ok(())
                }
                ProofCommand::Step(step) => self.check_step(step, &commands_stack, false),
                ProofCommand::Subproof {
                    commands: inner_commands,
                    assignment_args,
                    variable_args,
                } => {
                    let time = Instant::now();
                    let new_context = self.build_context(assignment_args, variable_args);
                    self.context.push(new_context);
                    commands_stack.push((0, inner_commands));

                    let step_index = inner_commands
                        .last()
                        .and_then(|s| match s {
                            ProofCommand::Step(s) => Some(s.index.clone()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    self.add_statistics_measurement(&step_index, "anchor*", time);
                    continue;
                }
                ProofCommand::Assume { index, term } => {
                    let time = Instant::now();

                    // Some subproofs contain "assume" commands inside them. These don't refer
                    // to the original problem premises, so we ignore the "assume" command if
                    // it is inside a subproof. Since the unit tests for the rules don't define the
                    // original problem, but sometimes use "assume" commands, we also skip the
                    // command if we are in a testing context.
                    let result = if self.config.is_running_test || commands_stack.len() > 1 {
                        Ok(())
                    } else {
                        let is_valid = proof.premises.contains(term)
                            || proof
                                .premises
                                .iter()
                                .any(|u| DeepEq::eq_modulo_reordering(term, u));
                        if is_valid {
                            Ok(())
                        } else {
                            Err(CheckerError {
                                // TODO: Add specific error for this
                                inner: RuleError::Unspecified,
                                rule_name: "assume".into(),
                                step: index.clone(),
                            })
                        }
                    };
                    self.add_statistics_measurement(index, "assume*", time);
                    result
                }
            }?;
            commands_stack.last_mut().unwrap().0 += 1;
        }

        Ok(())
    }

    fn check_step<'a>(
        &mut self,
        ProofStep {
            index,
            clause,
            rule: rule_name,
            premises,
            args,
            discharge: _, // The discharge attribute is not used when checking
        }: &'a ProofStep,
        commands_stack: &'a [(usize, &'a [ProofCommand])],
        is_end_of_subproof: bool,
    ) -> CheckerResult {
        let time = Instant::now();
        let rule = match Self::get_rule(rule_name) {
            Some(r) => r,
            None if self.config.skip_unknown_rules => return Ok(()),
            None => {
                return Err(CheckerError {
                    inner: RuleError::UnknownRule,
                    rule_name: rule_name.clone(),
                    step: index.clone(),
                })
            }
        };
        let premises = premises
            .iter()
            .map(|&(depth, i)| &commands_stack[depth].1[i])
            .collect();

        // If this step ends a subproof, it might need to implicitly reference the other commands
        // in the subproof. Therefore, we pass them via the `subproof_commands` field
        let subproof_commands = if is_end_of_subproof {
            Some(commands_stack.last().unwrap().1)
        } else {
            None
        };

        let rule_args = RuleArgs {
            conclusion: clause,
            premises,
            args,
            pool: &mut self.pool,
            context: &self.context,
            subproof_commands,
        };
        rule(rule_args).map_err(|e| CheckerError {
            inner: e,
            rule_name: rule_name.clone(),
            step: index.clone(),
        })?;
        self.add_statistics_measurement(index, rule_name, time);
        Ok(())
    }

    fn build_context(
        &mut self,
        assignment_args: &[(String, Rc<Term>)],
        variable_args: &[SortedVar],
    ) -> Context {
        // Since some rules (like "refl") need to apply substitutions until a fixed point, we
        // precompute these substitutions into a separate hash map. This assumes that the assignment
        // arguments are in the correct order.
        let mut substitutions = AHashMap::new();
        let mut substitutions_until_fixed_point = AHashMap::new();

        // We build the `substitutions_until_fixed_point` hash map from the bottom up, by using the
        // substitutions already introduced to transform the result of a new substitution before
        // inserting it into the hash map. So for instance, if the substitutions are "(:= y z)" and
        // "(:= x (f y))", we insert the first substitution, and then, when introducing the second,
        // we use the current state of the hash map to transform "(f y)" into "(f z)". The
        // resulting hash map will then contain "(:= y z)" and "(:= x (f z))"
        for (var, value) in assignment_args.iter() {
            let var_term = terminal!(var var; self.pool.add_term(Term::Sort(value.sort().clone())));
            let var_term = self.pool.add_term(var_term);
            substitutions.insert(var_term.clone(), value.clone());

            let new_value = self
                .pool
                .apply_substitutions(value, &substitutions_until_fixed_point);
            substitutions_until_fixed_point.insert(var_term, new_value);
        }

        // Some rules (notably "refl") need to apply the substitutions introduced by all the
        // previous contexts instead of just the current one. Instead of doing this iteratively
        // everytime the rule is used, we precompute the cumulative substitutions of this context
        // and all the previous ones and store that in a hash map. This improves the performance of
        // these rules considerably
        let mut cumulative_substitutions = substitutions_until_fixed_point.clone();
        if let Some(previous_context) = self.context.last() {
            for (k, v) in previous_context.cumulative_substitutions.iter() {
                let value = match substitutions_until_fixed_point.get(v) {
                    Some(new_value) => new_value,
                    None => v,
                };
                cumulative_substitutions.insert(k.clone(), value.clone());
            }
        }

        let bindings = variable_args.iter().cloned().collect();
        Context {
            substitutions,
            substitutions_until_fixed_point,
            cumulative_substitutions,
            bindings,
        }
    }

    fn add_statistics_measurement(&mut self, step_index: &str, rule: &str, start_time: Instant) {
        if let Some(stats) = &mut self.config.statistics {
            let measurement = start_time.elapsed();
            let file_name = stats.file_name.to_string();
            let step_index = step_index.to_string();
            let rule = rule.to_string();
            let id = StepId {
                file: file_name.clone().into_boxed_str(),
                step_index: step_index.into_boxed_str(),
                rule: rule.clone().into_boxed_str(),
            };
            stats.step_time.add(&id, measurement);
            stats
                .step_time_by_file
                .entry(file_name)
                .or_default()
                .add(&id, measurement);
            stats
                .step_time_by_rule
                .entry(rule)
                .or_default()
                .add(&id, measurement);
            *stats.checking_time += measurement;
        }
    }

    pub fn get_rule(rule_name: &str) -> Option<Rule> {
        use rules::*;

        // Converts a rule in the old format (returning `Option<()>`) to the new format (returning
        // `RuleResult`) by adding a `RuleError::Unspecified` error
        macro_rules! to_new_format {
            ($old:expr) => {
                |args| $old(args).ok_or(RuleError::Unspecified)
            };
        }

        Some(match rule_name {
            "true" => to_new_format!(tautology::r#true),
            "false" => to_new_format!(tautology::r#false),
            "not_not" => to_new_format!(tautology::not_not),
            "and_pos" => to_new_format!(tautology::and_pos),
            "and_neg" => to_new_format!(tautology::and_neg),
            "or_pos" => to_new_format!(tautology::or_pos),
            "or_neg" => to_new_format!(tautology::or_neg),
            "xor_pos1" => to_new_format!(tautology::xor_pos1),
            "xor_pos2" => to_new_format!(tautology::xor_pos2),
            "xor_neg1" => to_new_format!(tautology::xor_neg1),
            "xor_neg2" => to_new_format!(tautology::xor_neg2),
            "implies_pos" => to_new_format!(tautology::implies_pos),
            "implies_neg1" => to_new_format!(tautology::implies_neg1),
            "implies_neg2" => to_new_format!(tautology::implies_neg2),
            "equiv_pos1" => to_new_format!(tautology::equiv_pos1),
            "equiv_pos2" => to_new_format!(tautology::equiv_pos2),
            "equiv_neg1" => to_new_format!(tautology::equiv_neg1),
            "equiv_neg2" => to_new_format!(tautology::equiv_neg2),
            "ite_pos1" => to_new_format!(tautology::ite_pos1),
            "ite_pos2" => to_new_format!(tautology::ite_pos2),
            "ite_neg1" => to_new_format!(tautology::ite_neg1),
            "ite_neg2" => to_new_format!(tautology::ite_neg2),
            "eq_reflexive" => reflexivity::eq_reflexive,
            "eq_transitive" => to_new_format!(transitivity::eq_transitive),
            "eq_congruent" => congruence::eq_congruent,
            "eq_congruent_pred" => congruence::eq_congruent_pred,
            "distinct_elim" => to_new_format!(clausification::distinct_elim),
            "la_rw_eq" => to_new_format!(linear_arithmetic::la_rw_eq),
            "la_generic" => to_new_format!(linear_arithmetic::la_generic),
            "lia_generic" => to_new_format!(linear_arithmetic::lia_generic),
            "la_disequality" => to_new_format!(linear_arithmetic::la_disequality),
            "la_tautology" => to_new_format!(linear_arithmetic::la_tautology),
            "forall_inst" => to_new_format!(quantifier::forall_inst),
            "qnt_join" => to_new_format!(quantifier::qnt_join),
            "qnt_rm_unused" => to_new_format!(quantifier::qnt_rm_unused),
            "th_resolution" | "resolution" => to_new_format!(resolution::resolution),
            "refl" => reflexivity::refl,
            "trans" => to_new_format!(transitivity::trans),
            "cong" => congruence::cong,
            "and" => to_new_format!(clausification::and),
            "tautology" => to_new_format!(resolution::tautology),
            "not_or" => to_new_format!(clausification::not_or),
            "or" => to_new_format!(clausification::or),
            "not_and" => to_new_format!(clausification::not_and),
            "implies" => to_new_format!(clausification::implies),
            "not_implies1" => to_new_format!(clausification::not_implies1),
            "not_implies2" => to_new_format!(clausification::not_implies2),
            "equiv1" => to_new_format!(tautology::equiv1),
            "equiv2" => to_new_format!(tautology::equiv2),
            "not_equiv1" => to_new_format!(tautology::not_equiv1),
            "not_equiv2" => to_new_format!(tautology::not_equiv2),
            "ite1" => to_new_format!(tautology::ite1),
            "ite2" => to_new_format!(tautology::ite2),
            "not_ite1" => to_new_format!(tautology::not_ite1),
            "not_ite2" => to_new_format!(tautology::not_ite2),
            "ite_intro" => to_new_format!(tautology::ite_intro),
            "contraction" => to_new_format!(resolution::contraction),
            "connective_def" => to_new_format!(tautology::connective_def),
            "ite_simplify" => to_new_format!(simplification::ite_simplify),
            "eq_simplify" => to_new_format!(simplification::eq_simplify),
            "and_simplify" => to_new_format!(simplification::and_simplify),
            "or_simplify" => to_new_format!(simplification::or_simplify),
            "not_simplify" => to_new_format!(simplification::not_simplify),
            "implies_simplify" => to_new_format!(simplification::implies_simplify),
            "equiv_simplify" => to_new_format!(simplification::equiv_simplify),
            "bool_simplify" => to_new_format!(simplification::bool_simplify),
            "qnt_simplify" => to_new_format!(simplification::qnt_simplify),
            "div_simplify" => to_new_format!(simplification::div_simplify),
            "prod_simplify" => to_new_format!(simplification::prod_simplify),
            "minus_simplify" => to_new_format!(simplification::minus_simplify),
            "sum_simplify" => to_new_format!(simplification::sum_simplify),
            "comp_simplify" => to_new_format!(simplification::comp_simplify),
            "nary_elim" => to_new_format!(clausification::nary_elim),
            "ac_simp" => to_new_format!(simplification::ac_simp),
            "bfun_elim" => to_new_format!(clausification::bfun_elim),
            "bind" => to_new_format!(subproof::bind),
            "qnt_cnf" => to_new_format!(quantifier::qnt_cnf),
            "subproof" => to_new_format!(subproof::subproof),
            "let" => to_new_format!(subproof::r#let),
            "onepoint" => to_new_format!(subproof::onepoint),
            "sko_ex" => to_new_format!(subproof::sko_ex),
            "sko_forall" => to_new_format!(subproof::sko_forall),
            "reordering" => to_new_format!(extras::reordering),
            "symm" => to_new_format!(extras::symm),
            "not_symm" => to_new_format!(extras::not_symm),

            // Special rule that always checks as valid. It is mostly used in tests
            "trust" => |_| Ok(()),

            _ => return None,
        })
    }
}
