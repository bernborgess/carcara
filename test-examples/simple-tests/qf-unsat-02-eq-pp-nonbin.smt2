(set-logic QF_UF)
(set-info :smt-lib-version 2.0)
(set-info :category "crafted")
(set-info :status unsat)
(declare-sort U 0)
(declare-fun a () U)
(declare-fun b () U)
(declare-fun c () U)
(declare-fun d () U)
(declare-fun e1 () U)
(declare-fun e2 () U)
(declare-fun f (U U U) U)

(assert (= a b))
(assert (= c d))
(assert (= e1 e2))
(assert (not (= (f a c e1) (f b d e2))))
(check-sat)
(exit)
