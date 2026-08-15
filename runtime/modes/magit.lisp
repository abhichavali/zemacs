;;;; magit.lisp — the half of magit that is a question.
;;;;
;;;; The status buffer, the diffs, the staging and every destructive guard are
;;;; Rust, in `crates/app/src/magit.rs', and `docs/boundary.org' says why the
;;;; guards in particular cannot be anywhere else: they are a *funnel*, and a
;;;; check in a Lisp `magit-discard' would be walked straight past by `(magit
;;;; "discard")'.
;;;;
;;;; What is here is the other half of that same paragraph. A question whose
;;;; answer is *data* — a branch name, a revision, a stash message — is
;;;; `read-string', and belongs in the image, where the prompt is a continuation
;;;; and nothing blocks while it is open. So a git verb may carry one argument
;;;; after a space, and these are the dozen commands that put one there.
;;;;
;;;; The names are `git-...' and not `magit-...' on purpose, and it is core's
;;;; rule rather than a preference: `run_action' turns *every* name beginning
;;;; with `magit-' into an `EditorCommand::Git' before the image is ever asked,
;;;; so a Lisp function called `magit-checkout' could be defined here and never
;;;; called. `project-make' has the same shape for the same reason — see the
;;;; note at the top of `plugins/project.lisp'.
;;;;
;;;; ponytail: every one of these is a bare `read-string' rather than a
;;;; `completing-read' over the branches, because the branch list is git's and
;;;; the image has no reader for it. The upgrade is an `ask_here' arm in
;;;; `crates/lisp' — the route `latex-fragments' takes — plus a workspace edge
;;;; from `zemacs-lisp' to `zemacs-git'.

(in-package :zemacs)

(defun %git-ask (label verb)
  "Ask LABEL, then run the git VERB with the answer as its argument.

Nothing happens for an empty answer or a cancelled prompt, which is the same
`(when answer ...)' every other `read-string' caller does."
  (read-string label
    (lambda (answer)
      (let ((answer (and answer (string-trim " " answer))))
        (when (and answer (plusp (length answer)))
          (magit (format nil "~a ~a" verb answer))))))
  nil)

;;; ---------------------------------------------------------------------------
;;; Branches
;;;
;;; Four verbs and one question each. `b c' is Magit's create-and-switch, which
;;; is the one that gets pressed; `b n' makes the branch and stays where you are.

(defun git-checkout ()
  "Check out a branch, tag or commit by name."
  (%git-ask "Checkout: " "checkout"))

(defun git-branch-create ()
  "Make a branch at HEAD and move onto it."
  (%git-ask "Create and checkout branch: " "checkout-new"))

(defun git-branch-new ()
  "Make a branch at HEAD without moving onto it."
  (%git-ask "Create branch: " "branch-create"))

(defun git-branch-delete ()
  "Delete a branch, refusing if it is not merged anywhere else."
  (%git-ask "Delete branch: " "branch-delete"))

(defun git-branch-delete-force ()
  "Delete a branch even if unmerged. Asks again before it does."
  (%git-ask "Delete branch (even if unmerged): " "branch-delete-force"))

;;; ---------------------------------------------------------------------------
;;; The rest of the arguments
;;;
;;; Everything that acts on *the commit under the cursor* — cherry-pick, revert,
;;; reset, squash, drop, reword — needs no question at all and is bound straight
;;; to its verb: the log section is right there, and pointing at a line is a
;;; better answer than typing a hash. These are the ones with nothing to point
;;; at.

(defun git-merge ()
  "Merge a branch into this one."
  (%git-ask "Merge: " "merge"))

(defun git-rebase-onto ()
  "Replay everything this branch has that another one does not onto that one."
  (%git-ask "Rebase onto: " "rebase"))

(defun git-stash-named ()
  "Stash the working tree under a message you choose."
  (%git-ask "Stash message: " "stash"))

(defun git-reset-hard ()
  "Reset --hard to a revision you name. Asks again before it does."
  (%git-ask "Reset --hard to: " "reset-hard"))

;;; ---------------------------------------------------------------------------
;;; The keymap
;;;
;;; Here rather than in `init.lisp' so that a config written a year ago gets
;;; them: `*runtime-modules*' is what carries an addition to every zemacs, and
;;; a copied `init.lisp' is frozen the day it is copied. The handful of keys
;;; that were already in `init.lisp' — s, u, S, U, TAB, q, g r — stay there; a
;;; binding is just a name in a table, so the two files add to one keymap.
;;;
;;; The prefixes are Magit's own letters, and which-key draws the popup for
;;; each of them out of this table with nothing else asked for: pressing `b'
;;; lists what continues it, which is what a transient *is* here.

;;; `RET' visits the file under the cursor. `k' throws its changes away, and is
;;; the one key here that always stops to ask.
(define-key "magit" "<ret>" "magit-visit")
(define-key "magit" "k" "magit-discard")

;;; --- and the same two keys again, where a selection can reach them ---------
;;;
;;; `"magit"' above is the *state* keymap, and a state keymap layers over Normal
;;; only. Press `V' to select the three lines of a hunk you actually want and the
;;; editor is in Visual — where magit has stopped answering, which is precisely
;;; the moment you need it to. `"magit-mode"' is a *minor* mode, pushed onto the
;;; status buffer by `Magit::render', and a minor-mode binding is consulted in
;;; every editing mode.
;;;
;;; Two keys and not the whole table, because a minor mode this broad shadows the
;;; grammar wherever it reaches. `s' and `u' have no meaning in Visual worth
;;; keeping — vim's visual `s' is substitute, and you do not edit a generated
;;; status buffer — while every motion does.
;;;
;;; `k' is deliberately absent, and this is the important line. In Visual `k' is
;;; *up*: it is how the selection gets made in the first place, so binding it
;;; here would make building a selection impossible — and it would do it by
;;; discarding your work, since `k' is the destructive one. Region-discard is
;;; `M-x magit-discard', which is the right amount of friction for it.
(define-key "magit-mode" "s" "magit-stage")
(define-key "magit-mode" "u" "magit-unstage")

;;; Commit. `c' has to be a prefix rather than a whole binding — core resolves
;;; an exact binding before it asks whether a sequence is a prefix, so while `c'
;;; alone meant commit, `c a' could never be typed.
(define-key "magit" "c c" "magit-commit")
(define-key "magit" "c a" "magit-commit-amend")   ; fold the index in, new message
(define-key "magit" "c e" "magit-amend")          ; extend: fold it in, keep it
(define-key "magit" "c w" "magit-reword")         ; the commit at point

;;; Branches.
(define-key "magit" "b b" "git-checkout")
(define-key "magit" "b c" "git-branch-create")
(define-key "magit" "b n" "git-branch-new")
(define-key "magit" "b k" "git-branch-delete")
(define-key "magit" "b K" "git-branch-delete-force")

;;; The stash. `z p' and `z a' act on the entry under the cursor when there is
;;; one, and on the top of the stack when there is not.
(define-key "magit" "z z" "magit-stash")
(define-key "magit" "z s" "git-stash-named")
(define-key "magit" "z p" "magit-stash-pop")
(define-key "magit" "z a" "magit-stash-apply")
(define-key "magit" "z k" "magit-stash-drop")

;;; Fetching and pushing. `P' and `F' are prefixes for the same reason `c' is.
(define-key "magit" "f f" "magit-fetch")
(define-key "magit" "F F" "magit-pull")
(define-key "magit" "P p" "magit-push")
(define-key "magit" "P u" "magit-push-upstream")  ; the first push of a branch
(define-key "magit" "P f" "magit-push-force")     ; --force-with-lease, and asks

;;; Rebase, and the surgery on one commit that is a rebase underneath. The
;;; commit is the one under the cursor.
(define-key "magit" "r e" "git-rebase-onto")
(define-key "magit" "r w" "magit-reword")
(define-key "magit" "r f" "magit-squash")         ; fold into its parent
(define-key "magit" "r k" "magit-drop-commit")

;;; Merge, cherry-pick, revert — and the two verbs that finish or undo any of
;;; them, a rebase included. git spells `--continue' and `--abort' the same way
;;; for all four and the repository knows which one is running, so these are one
;;; pair of keys under each popup rather than four.
(define-key "magit" "m m" "git-merge")
(define-key "magit" "m c" "magit-continue")
(define-key "magit" "m a" "magit-abort")
(define-key "magit" "A A" "magit-cherry-pick")
(define-key "magit" "A a" "magit-abort")
(define-key "magit" "V V" "magit-revert")
(define-key "magit" "V a" "magit-abort")

;;; Reset, onto the commit under the cursor. Only `X h' can lose work, and it
;;; is the one that asks.
(define-key "magit" "X s" "magit-reset-soft")
(define-key "magit" "X m" "magit-reset-mixed")
(define-key "magit" "X h" "magit-reset-hard")
(define-key "magit" "X H" "git-reset-hard")       ; ...to a revision you name

;;; Conflicts. Staging a conflicted file is how you say you fixed it by hand;
;;; these two are how you say to take one side whole, and both ask first.
(define-key "magit" "e o" "magit-resolve-ours")
(define-key "magit" "e t" "magit-resolve-theirs")

;;; More log in the section that is already showing some, and back again.
(define-key "magit" "l l" "magit-log")
