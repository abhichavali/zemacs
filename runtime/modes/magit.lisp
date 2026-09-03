;;;; magit.lisp — the half of magit that is a question, and the keymap.
;;;;
;;;; The status buffer, the diffs, the staging and every destructive guard are
;;;; Rust, in `crates/app/src/magit.rs', and `docs/boundary.org' says why the
;;;; guards in particular cannot be anywhere else: they are a *funnel*, and a
;;;; check in a Lisp `magit-discard' would be walked straight past by `(magit
;;;; "discard")'.
;;;;
;;;; What is here is the other half of that same paragraph. A question whose
;;;; answer is *data* — a branch name, a revision, a stash message — is
;;;; `read-string' or `completing-read', and belongs in the image, where the
;;;; prompt is a continuation and nothing blocks while it is open. So a git verb
;;;; may carry one argument after a space, and these are the dozen-odd commands
;;;; that put one there.
;;;;
;;;; The names are `git-...' and not `magit-...' on purpose, and it is core's
;;;; rule rather than a preference: `run_action' turns *every* name beginning
;;;; with `magit-' into an `EditorCommand::Git' before the image is ever asked,
;;;; so a Lisp function called `magit-checkout' could be defined here and never
;;;; called. `project-make' has the same shape for the same reason — see the
;;;; note at the top of `plugins/project.lisp'.
;;;;
;;;; A question about a *revision* completes over `(git-refs)' — every local
;;;; branch, remote branch and tag of the repository behind the live buffer — and
;;;; one about a branch you already have over `(git-branches)'. Both are readers
;;;; in `crates/lisp', the route `latex-fragments' takes. Nothing has to match:
;;;; a commit hash typed into `Checkout:' checks out that commit.
;;;;
;;;; Three more things live here because they are keys and prose rather than
;;;; git: the labels which-key writes beside each key, which are Magit's own
;;;; transient wording; `?', which shows the whole table; and `git-rebase-mode',
;;;; the buffer an interactive rebase edits its todo list in.

(in-package :zemacs)

(defun %git-ask (label verb &optional candidates)
  "Ask LABEL, then run the git VERB with the answer as its argument.

CANDIDATES, when given, is a function answering what to complete over, and the
prompt becomes a picker. Nothing happens for an empty answer or a cancelled
prompt, which is the same `(when answer ...)' every other `read-string' caller
does."
  (let ((k (lambda (answer)
             (let ((answer (and answer (string-trim " " answer))))
               (when (and answer (plusp (length answer)))
                 (magit (format nil "~a ~a" verb answer)))))))
    (if candidates
        (completing-read label (funcall candidates) k)
        (read-string label k)))
  nil)

;;; ---------------------------------------------------------------------------
;;; Branches
;;;
;;; `b c' is Magit's create-and-switch, which is the one that gets pressed; `b
;;; n' makes the branch and stays where you are.

(defun git-checkout ()
  "Check out a branch, tag or commit by name."
  (%git-ask "Checkout: " "checkout" #'git-refs))

(defun git-branch-create ()
  "Make a branch at HEAD and move onto it."
  (%git-ask "Create and checkout branch: " "checkout-new"))

(defun git-branch-new ()
  "Make a branch at HEAD without moving onto it."
  (%git-ask "Create branch: " "branch-create"))

(defun git-branch-delete ()
  "Delete a branch, refusing if it is not merged anywhere else."
  (%git-ask "Delete branch: " "branch-delete" #'git-branches))

(defun git-branch-delete-force ()
  "Delete a branch even if unmerged. Asks again before it does."
  (%git-ask "Delete branch (even if unmerged): " "branch-delete-force" #'git-branches))

(defun git-branch-rename ()
  "Give the current branch a new name."
  (%git-ask "Rename branch to: " "branch-rename"))

;;; ---------------------------------------------------------------------------
;;; The rest of the arguments
;;;
;;; Everything that acts on *the commit under the cursor* — cherry-pick, revert,
;;; reset, squash, drop, reword, fixup — needs no question at all and is bound
;;; straight to its verb: the log section is right there, and pointing at a
;;; line is a better answer than typing a hash. These are the ones with nothing
;;; to point at.

(defun git-merge ()
  "Merge a branch into this one."
  (%git-ask "Merge: " "merge" #'git-refs))

(defun git-rebase-onto ()
  "Replay everything this branch has that another one does not onto that one."
  (%git-ask "Rebase onto: " "rebase" #'git-refs))

(defun git-rebase-autosquash ()
  "Fold every fixup! commit into the commit it names, back to a base."
  (%git-ask "Autosquash onto: " "rebase-autosquash" #'git-refs))

(defun git-stash-named ()
  "Stash the working tree under a message you choose."
  (%git-ask "Stash message: " "stash"))

(defun git-reset-hard ()
  "Reset --hard to a revision you name. Asks again before it does."
  (%git-ask "Reset --hard to: " "reset-hard" #'git-refs))

(defun git-tag ()
  "Tag the commit under the cursor, or HEAD."
  (%git-ask "Tag name: " "tag"))

(defun git-tag-delete ()
  "Delete a tag. The commit stays."
  (%git-ask "Delete tag: " "tag-delete" #'git-refs))

;;; ---------------------------------------------------------------------------
;;; The keymap
;;;
;;; Here rather than in `init.lisp' so that a config written a year ago gets
;;; them: `*runtime-modules*' is what carries an addition to every zemacs, and
;;; a copied `init.lisp' is frozen the day it is copied. The handful of keys
;;; that were already in `init.lisp' — s, u, S, U, TAB, q, g r, r c, r s, r a —
;;; stay there; a binding is just a name in a table, so the two files add to one
;;; keymap.
;;;
;;; The prefixes are Magit's own letters, and which-key draws the popup for
;;; each of them out of this table with nothing else asked for: pressing `b'
;;; lists what continues it, which is what a transient *is* here. The third
;;; argument of `%magit-key' is what that popup says beside the key — Magit's
;;; own transient wording, so `c F' reads as "Instant fixup" and not as a verb
;;; name nobody chose.

(defun %magit-key (keys command label)
  "Bind KEYS to COMMAND in the status buffer, and have which-key call it LABEL."
  (define-key "magit" keys command)
  (when (fboundp 'which-key-describe) (which-key-describe command label)))

;;; What the prefixes are called while they are still prefixes.
(when (boundp '*which-key-groups*)
  (dolist (group '(("b" . "branch") ("c" . "commit") ("f" . "fetch") ("F" . "pull")
                   ("P" . "push") ("r" . "rebase") ("m" . "merge") ("A" . "cherry-pick")
                   ("V" . "revert") ("X" . "reset") ("z" . "stash") ("t" . "tag")
                   ("e" . "resolve") ("l" . "log") ("g" . "refresh")))
    (setf *which-key-groups*
          (cons group (remove (car group) *which-key-groups* :key #'car :test #'string=)))))

;;; `RET' visits the file under the cursor. `x' throws its changes away, and is
;;; the one key here that always stops to ask.
;;;
;;; `x' and not magit's own `k', and this is the one entry in the table that was
;;; a bug rather than a preference. `"magit"' layers over Normal *so that the
;;; motions keep working* — and `k' is `up'. Bound here it stopped being a motion
;;; in the one buffer whose entire interface is "point at a line": moving up the
;;; staging area opened a discard prompt instead of moving, and the `j' after it
;;; went into that prompt rather than the buffer, which is why the symptom read
;;; as "sometimes j and k do nothing" rather than as "k discards". It was
;;; destructive by default besides — the accident it invites is the one key here
;;; that can lose work.
;;;
;;; `evil-collection-magit' moves discard to `x' for this reason and it is the
;;; spelling to match: vim's `x' is delete-character, which a generated buffer
;;; has no use for, so nothing is displaced by taking it.
(%magit-key "<ret>" "magit-visit" "Visit")
(%magit-key "x" "magit-discard" "Discard")

;;; `?' and `h' are Magit's `magit-dispatch': the whole table at once, drawn by
;;; which-key out of this file. `h' is a motion in Normal and a useless one in a
;;; buffer with nothing to the left, so nothing is lost by taking it.
(defun git-dispatch ()
  "Show every key the status buffer answers to."
  (when (fboundp 'which-key-show)
    ;; The status buffer's own keys: what its two maps hold, less every row
    ;; Normal holds identically — `define-key-everywhere' writes `M-x' and the
    ;; zoom keys into this map too, and a table of git verbs is not where
    ;; anyone looks those up. The whole of it, not the first eighteen rows:
    ;; this is the one panel that is asked for rather than shown in passing.
    (let* ((everywhere (which-key-rows "" '("normal")))
           (rows (remove-if (lambda (row) (equal row (assoc (car row) everywhere
                                                            :test #'string=)))
                            (which-key-rows "" '("magit-mode" "magit"))))
           (*which-key-limit* 60))
      (which-key-show "" rows)))
  nil)
(%magit-key "?" "git-dispatch" "Help")
(%magit-key "h" "git-dispatch" "Help")

;;; --- and the two keys again, where a selection can reach them --------------
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
;;; Discard is deliberately absent, and this is the important line. It is the one
;;; key here that can lose work, and a minor-mode binding is consulted in *every*
;;; editing mode — so putting it in this table would arm it while you are still
;;; building the selection you meant to act on. Region-discard is
;;; `M-x magit-discard', which is the right amount of friction for it.
(define-key "magit-mode" "s" "magit-stage")
(define-key "magit-mode" "u" "magit-unstage")
(when (fboundp 'which-key-describe)
  (which-key-describe "magit-stage" "Stage")
  (which-key-describe "magit-unstage" "Unstage")
  (which-key-describe "magit-stage-all" "Stage all")
  (which-key-describe "magit-unstage-all" "Unstage all")
  (which-key-describe "magit-toggle" "Toggle section")
  (which-key-describe "magit-refresh" "Refresh")
  (which-key-describe "magit-status" "Status")
  (which-key-describe "show-dashboard" "Quit"))

;;; Commit. `c' has to be a prefix rather than a whole binding — core resolves
;;; an exact binding before it asks whether a sequence is a prefix, so while `c'
;;; alone meant commit, `c a' could never be typed.
;;;
;;; `c f' makes a `fixup!' of the commit under the cursor out of what is staged,
;;; for `r f' to fold in later; `c F' does both at once, which is the gesture
;;; "this belongs in that commit" and the most-pressed key in Magit that is not
;;; `s'.
(%magit-key "c c" "magit-commit" "Commit")
(%magit-key "c a" "magit-commit-amend" "Amend")            ; fold the index in, new message
(%magit-key "c e" "magit-amend" "Extend")                  ; fold it in, keep the message
(%magit-key "c w" "magit-reword" "Reword")                 ; the commit at point
(%magit-key "c f" "magit-commit-fixup" "Fixup")
(%magit-key "c F" "magit-commit-instant-fixup" "Instant fixup")

;;; Branches.
(%magit-key "b b" "git-checkout" "Checkout")
(%magit-key "b c" "git-branch-create" "Checkout new branch")
(%magit-key "b n" "git-branch-new" "Create new branch")
(%magit-key "b m" "git-branch-rename" "Rename")
(%magit-key "b k" "git-branch-delete" "Delete")
(%magit-key "b K" "git-branch-delete-force" "Delete (force)")

;;; The stash. `z p' and `z a' act on the entry under the cursor when there is
;;; one, and on the top of the stack when there is not.
(%magit-key "z z" "magit-stash" "Stash")
(%magit-key "z s" "git-stash-named" "Stash with message")
(%magit-key "z p" "magit-stash-pop" "Pop")
(%magit-key "z a" "magit-stash-apply" "Apply")
(%magit-key "z k" "magit-stash-drop" "Drop")

;;; Fetching, pulling and pushing. `P' and `F' are prefixes for the same reason
;;; `c' is. Magit tells the push-remote (`p') from the upstream (`u') and these
;;; do not — a repository with one remote has one of each — so the two letters
;;; are two spellings of one verb, kept because fingers know them.
(%magit-key "f f" "magit-fetch" "Fetch")
(%magit-key "f u" "magit-fetch" "Fetch from upstream")
(%magit-key "f p" "magit-fetch" "Fetch from push-remote")
(%magit-key "f a" "magit-fetch-all" "Fetch all remotes")
(%magit-key "F F" "magit-pull" "Pull")
(%magit-key "F u" "magit-pull" "Pull from upstream")
(%magit-key "F p" "magit-pull" "Pull from push-remote")
(%magit-key "P P" "magit-push" "Push")
(%magit-key "P p" "magit-push" "Push to push-remote")
(%magit-key "P u" "magit-push-upstream" "Push, setting upstream")   ; the first push of a branch
(%magit-key "P f" "magit-push-force" "Push (force-with-lease)")     ; and asks

;;; Rebase, and the surgery on one commit that is a rebase underneath. The
;;; commit is the one under the cursor. `r i' opens the todo list in a buffer
;;; of its own — `git-rebase-mode', below — and `r m' is the one-step version
;;; of it: stop at this commit so it can be amended.
;;;
;;; `r c', `r s' and `r a' — continue, skip, abort — are in `init.lisp'; `r r'
;;; is Magit's spelling of continue and is added here so both hands are right.
(%magit-key "r i" "magit-rebase-interactive" "Interactively")
(%magit-key "r e" "git-rebase-onto" "Onto elsewhere")
(%magit-key "r u" "magit-rebase-upstream" "Onto upstream")
(%magit-key "r p" "magit-rebase-upstream" "Onto push-remote")
(%magit-key "r m" "magit-rebase-modify" "Modify a commit")
(%magit-key "r w" "magit-reword" "Reword a commit")
(%magit-key "r k" "magit-drop-commit" "Remove a commit")
(%magit-key "r f" "git-rebase-autosquash" "Autosquash")
(%magit-key "r r" "magit-rebase-continue" "Continue")
(when (fboundp 'which-key-describe)
  (which-key-describe "magit-rebase-continue" "Continue")
  (which-key-describe "magit-rebase-skip" "Skip")
  (which-key-describe "magit-rebase-abort" "Abort")
  ;; Not bound: squash-into-parent is `M-x magit-squash', since Magit has no
  ;; key for it and `r f' is autosquash there.
  (which-key-describe "magit-squash" "Squash into parent"))

;;; Merge, cherry-pick, revert — and the two verbs that finish or undo any of
;;; them, a rebase included. git spells `--continue' and `--abort' the same way
;;; for all four and the repository knows which one is running, so these are one
;;; pair of keys under each popup rather than four.
(%magit-key "m m" "git-merge" "Merge")
(%magit-key "m c" "magit-continue" "Continue")
(%magit-key "m a" "magit-abort" "Abort")
(%magit-key "A A" "magit-cherry-pick" "Cherry-pick")
(%magit-key "A a" "magit-abort" "Abort")
(%magit-key "V V" "magit-revert" "Revert")
(%magit-key "V a" "magit-abort" "Abort")

;;; Reset, onto the commit under the cursor. Only `X h' can lose work, and it
;;; is the one that asks.
(%magit-key "X s" "magit-reset-soft" "Soft")
(%magit-key "X m" "magit-reset-mixed" "Mixed")
(%magit-key "X h" "magit-reset-hard" "Hard")
(%magit-key "X H" "git-reset-hard" "Hard, to a revision")

;;; Tags.
(%magit-key "t t" "git-tag" "Tag")
(%magit-key "t k" "git-tag-delete" "Delete")

;;; Conflicts. Staging a conflicted file is how you say you fixed it by hand;
;;; these two are how you say to take one side whole, and both ask first.
(%magit-key "e o" "magit-resolve-ours" "Take ours")
(%magit-key "e t" "magit-resolve-theirs" "Take theirs")

;;; More log in the section that is already showing some, and back again.
(%magit-key "l l" "magit-log" "More log")

;;; ---------------------------------------------------------------------------
;;; git-rebase-mode: the todo list of an interactive rebase
;;;
;;; `r i' asks Rust for the plan and Rust opens it as a buffer of this mode,
;;; looking exactly like the file `git rebase -i' would have handed an editor.
;;; The keys are `evil-collection''s for that file: a letter sets the step under
;;; point and moves down, `M-k'/`M-j' move a step, `C-c C-c' hands the list back
;;; to git and `C-c C-k' throws it away. Every edit is an ordinary buffer edit —
;;; `u' undoes it, Insert mode types into it — because the buffer *is* the todo
;;; list: `magit-rebase-finish' parses whatever it says.
;;;
;;; The same buffer could have been a form or a special mode with its own model,
;;; and git-rebase.el is the argument against: the file format is eight words,
;;; and the editor already knows how to edit a line.

(defparameter *rebase-actions*
  '("pick" "reword" "edit" "squash" "fixup" "drop" "exec" "break")
  "Every step git accepts. Their first letters are distinct, which is why the
one-letter spellings a hand-edited file is full of can be recognised by them.")

(defun %rebase-word (line)
  "LINE's first word."
  (subseq line 0 (or (position #\Space line) (length line))))

(defun %rebase-step-p (line)
  "True when LINE is a step: its first word is an action, whole or abbreviated."
  (let ((word (%rebase-word line)))
    (and (plusp (length word))
         (find-if (lambda (action)
                    (or (string= word action)
                        (and (= (length word) 1)
                             (char= (char word 0) (char action 0)))))
                  *rebase-actions*)
         t)))

(defun %rebase-face (start end face)
  (let ((ov (make-overlay start end)))
    (when ov (overlay-put ov 'face face))))

(defun %rebase-refresh-faces ()
  "Colour the list: the action as a keyword, the hash as a constant, the
legend as comments. Overlays rather than a language, because the buffer has
none — and re-laid from scratch after every edit this file makes, which is
nothing on a list this short.

ponytail: a line typed by hand in Insert is plain until the next action key;
the upgrade is an after-change hook, which the editor does not offer."
  (remove-overlays)
  (loop for n from 1 to (line-count)
        for line = (line-string n)
        for start = (line-start n)
        do (cond ((and (plusp (length line)) (char= (char line 0) #\#))
                  (%rebase-face start (+ start (length line)) "comment"))
                 ((%rebase-step-p line)
                  (let* ((word (length (%rebase-word line)))
                         (hash-end (and (< word (length line))
                                        (position #\Space line :start (1+ word)))))
                    (%rebase-face start (+ start word) "keyword")
                    (when hash-end
                      (%rebase-face (+ start word 1) (+ start hash-end) "constant")))))))

(define-derived-mode git-rebase-mode nil
  (%rebase-refresh-faces))

(defun %rebase-set-action (action)
  "Make the step under point ACTION, then move down a line — git-rebase.el's
own behaviour, so `s s s' over three commits reads as one gesture."
  (let* ((n (line-number))
         (line (line-string n)))
    (if (%rebase-step-p line)
        (let ((start (line-start n)))
          (replace-region start (+ start (length (%rebase-word line))) action)
          (%rebase-refresh-faces)
          (goto-line (min (1+ n) (line-count))))
        (message "not a rebase step"))))

(defun git-rebase-pick () "Use the commit as it is." (%rebase-set-action "pick"))
(defun git-rebase-reword () "Use the commit, but stop to edit its message." (%rebase-set-action "reword"))
(defun git-rebase-edit () "Use the commit, but stop to amend it." (%rebase-set-action "edit"))
(defun git-rebase-squash () "Meld the commit into the one before it, keeping both messages." (%rebase-set-action "squash"))
(defun git-rebase-fixup () "Meld the commit into the one before it, discarding its message." (%rebase-set-action "fixup"))
(defun git-rebase-drop () "Leave the commit out." (%rebase-set-action "drop"))

(defun %rebase-move (delta)
  "Swap the step under point with the one DELTA lines away, and follow it. Only
between steps: a step moved into the legend would be a step git never reads."
  (let* ((n (line-number))
         (m (+ n delta)))
    (if (and (<= 1 m (line-count))
             (%rebase-step-p (line-string n))
             (%rebase-step-p (line-string m)))
        (let ((a (min n m)) (b (max n m)))
          (replace-region (line-start a) (line-end b)
                          (format nil "~a~%~a" (line-string b) (line-string a)))
          (%rebase-refresh-faces)
          (goto-line m))
        (message "nothing to swap with"))))

(defun git-rebase-move-up () "Move this step up one." (%rebase-move -1))
(defun git-rebase-move-down () "Move this step down one." (%rebase-move 1))

(defun %rebase-insert-after (text)
  "A new line reading TEXT under the one point is on, and point onto it."
  (let ((at (min (point-max) (1+ (line-end)))))
    (insert-at at (format nil "~a~%" text))
    (%rebase-refresh-faces)
    (goto-char at)))

(defun git-rebase-exec ()
  "Run a shell command after this step."
  (read-string "Execute: "
    (lambda (command)
      (let ((command (and command (string-trim " " command))))
        (when (and command (plusp (length command)))
          (%rebase-insert-after (format nil "exec ~a" command))))))
  nil)

(defun git-rebase-break ()
  "Stop after this step, to look around."
  (%rebase-insert-after "break"))

(define-key "git-rebase-mode" "p" "git-rebase-pick")
(define-key "git-rebase-mode" "r" "git-rebase-reword")
(define-key "git-rebase-mode" "e" "git-rebase-edit")
(define-key "git-rebase-mode" "s" "git-rebase-squash")
(define-key "git-rebase-mode" "f" "git-rebase-fixup")
(define-key "git-rebase-mode" "d" "git-rebase-drop")
(define-key "git-rebase-mode" "x" "git-rebase-exec")
(define-key "git-rebase-mode" "b" "git-rebase-break")
(define-key "git-rebase-mode" "M-k" "git-rebase-move-up")
(define-key "git-rebase-mode" "M-j" "git-rebase-move-down")
(define-key "git-rebase-mode" "C-c C-c" "magit-rebase-finish")
(define-key "git-rebase-mode" "C-c C-k" "magit-rebase-cancel")
