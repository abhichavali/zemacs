;;;; avy — jump anywhere on screen by pressing a letter.
;;;;
;;;; Type the character you want to land on; every one of them on screen grows a
;;;; label over the top of it; press the label. Two keystrokes to anywhere the
;;;; eye can already see, which is the whole of the idea and the reason avy
;;;; beats a search for the jumps that are *short* — the ones where knowing the
;;;; pattern is more work than seeing the target.
;;;;
;;;; Entirely Lisp, and it is worth saying what that means here: the labels are
;;;; ordinary `display' overlays, so they follow the theme, move with the text
;;;; and die with it, and the renderer has never heard the word avy. The only
;;;; thing this file could not do for itself was *read one keystroke*, and the
;;;; three existing surfaces of this shape each record why their answer was
;;;; wrong for the next one:
;;;;
;;;;   * `read-string' / `completing-read' open a prompt, and a prompt draws a
;;;;     minibuffer and owns the keyboard. avy's gesture is one key pressed
;;;;     against the *document* — the same objection which-key and corfu make.
;;;;   * `ace-window' does read one key, in `dispatch_key', and resolves it
;;;;     itself into a window id core owns. A jump target is a buffer offset hung
;;;;     off an overlay, which lives *here*; core resolving it would mean core
;;;;     keeping a second copy of the label table this file already has.
;;;;   * A transient keymap — bind the label letters, enable, disable — is the
;;;;     lazy answer and is wrong on the one case that matters: a keymap catches
;;;;     the keys that are *in* it, so a key that is not a label falls through
;;;;     and edits the buffer with a screenful of labels still up. "Anything else
;;;;     cancels" is not something a keymap can say.
;;;;
;;;; So `grab-key' (crates/lisp/src/shim.c, `EditorCommand::GrabKey'): the next
;;;; keystroke, whatever it is, arrives here as `(avy-pick "a")' spelled the way
;;;; `key-bindings' spells a key. Core forgets before it calls, so no path
;;;; through this file can leave the keyboard captured, and a key that is not a
;;;; label arrives too — which is what makes cancelling *total* rather than
;;;; something this file has to remember to do.
;;;;
;;;; The ceiling, named rather than discovered: overlays are a `Vec' scanned
;;;; linearly per drawn line (crates/core/src/overlay.rs says so, and names avy
;;;; while doing it). One per candidate is exactly what that was sized against,
;;;; so candidates are scoped to the *visible* region and capped at the number of
;;;; labels there are letters for — which is also the honest bound, since an
;;;; unlabelled candidate is not a candidate.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; Policy

(defparameter *avy-keys* "asdfghjklqweruiopzxcvbnm"
  "The label alphabet, strongest key first: home row, then the top row, then the
bottom. Also the cap on how many candidates get labelled — a label is what makes
a candidate reachable, so running out of letters and running out of candidates
are the same event.

`t' and `y' are missing on purpose: they are the two Normal-mode keys whose
muscle memory is strongest at a moment when your hand is already reaching for a
position, and a mistyped label that yanks is a worse accident than one that
jumps to the wrong `e'.")

(defparameter *avy-scope* :word
  "Which occurrences of the typed character count.

  :word  only where it begins a word — avy's `avy-goto-word-1', and the default
         because it is what keeps the candidate count under the label count on a
         normal screen of prose or code;
  :char  every occurrence — avy's `avy-goto-char', denser and more precise.

Line starts are the third candidate rule avy has and are deliberately not a
value here: `avy-goto-line' reads no character at all, so it is a different
gesture rather than this one's policy. It would be this file with the first
`grab-key' taken out.")

(defparameter *avy-face* "keyword"
  "Face the labels are drawn in, from `face-list'. A face name and not a colour,
so the labels follow `load-theme' like every other overlay — and `keyword' for
`show-paren.lisp''s reason: there is no `avy-label' face, Lisp cannot add one,
and spending `set-syntax-color' on a spare would change every keyword in every
buffer to light up a dozen characters. For labels that shout rather than glow,
add `(overlay-put ov 'background *avy-face*)' in `%avy-label' and set this to
\"default\"; it is a stronger mark and it is at the mercy of the theme's
contrast, which is why it is not what ships.")

(defparameter *avy-bold* t
  "Whether labels are drawn bold as well as coloured. A label sits *inside* the
text it replaces, so it needs to read as not-text at a glance.")

;;; ---------------------------------------------------------------------------
;;; State
;;;
;;; DEFVAR and not DEFPARAMETER, for `which-key.lisp''s reason: a config reload
;;; re-reads this file, and re-binding these would forget overlays that are on
;;; screen — the one failure this feature must not have.

(defvar *avy-overlays* nil
  "The labels currently on screen, as overlay handles. This file's own, so it
takes its own off and leaves org-modern's bullets and show-paren's brackets
alone — `remove-overlays' would eat all three.")

(defvar *avy-state* nil
  "NIL, :char while waiting for the character to search for, or :label while
waiting for a label. The two `grab-key' turns are one function on the other side,
so the state is what tells them apart.")

;;; ---------------------------------------------------------------------------
;;; The scan

(defun %avy-region ()
  "(BEG . END) of the text the window is showing, as character offsets.

The whole of the ceiling story in one function. `window-scroll' is the first
visible line and `window-height' how many fit, both parked on the editor by the
renderer — so a buffer of ten thousand lines costs the same as one of forty.

ponytail: `window-scroll' is a *buffer* line and the renderer counts *visual*
rows, so a screen full of wrapped lines or an open fold over-estimates the
bottom and labels a few candidates below the fold. Over-labelling is harmless —
the label is simply not on screen to press — and the exact answer needs the
renderer to report the last row it drew, which is a field, not a fix here."
  (let* ((top (window-scroll))
         (last (min (line-count) (+ top (max 1 (window-height))))))
    (cons (line-start (1+ top)) (line-end last))))

(defun %avy-word-start-p (text i)
  "True when TEXT's character I begins a word. Alphanumeric-or-not, which is the
same rule `\\<' has and needs no syntax table to state."
  (or (zerop i) (not (alphanumericp (char text (1- i))))))

(defun %avy-candidates (char beg end)
  "Offsets in BEG..END where CHAR occurs, per `*avy-scope*', in document order.

One `buffer-substring' and a walk, rather than a `search-forward' per hit: the
region is a screenful, and the hits are what the walk is looking for anyway.
CHAR-EQUAL, so a lower-case key finds a capital — avy's `avy-case-fold-search',
and the looseness you want when the thing you are aiming at is one you can see."
  (let ((text (buffer-substring beg end))
        (out nil))
    (dotimes (i (length text) (nreverse out))
      (when (and (char-equal (char text i) char)
                 (or (eq *avy-scope* :char) (%avy-word-start-p text i)))
        (push (+ beg i) out)))))

;;; ---------------------------------------------------------------------------
;;; Labels

(defun %avy-label (pos label)
  "Put LABEL over the single character at POS. Answers the overlay, or NIL."
  (let ((ov (and (< pos (point-max)) (make-overlay pos (1+ pos)))))
    (when ov
      (overlay-put ov 'display label)
      (overlay-put ov 'face *avy-face*)
      (when *avy-bold* (overlay-put ov 'weight 'bold))
      ;; Read by nothing in Rust — the renderer draws four properties and this is
      ;; not one of them, so it never leaves the image. It is how a keystroke
      ;; finds its overlay, and it is why core needs no label table: there is
      ;; exactly one place the mapping lives and it is on the thing that draws it.
      (overlay-put ov 'avy-label label))
    ov))

(defun avy-cancel ()
  "Take every label off and leave point exactly where it was.

Total, and that is the requirement rather than a nicety: a half-cleared screen of
labels is this feature's worst failure. Every path out goes through here — the
jump, a key that is no label, a second `avy-goto-char', and by hand from M-x —
and `grab-key' with no argument is the belt for the last of those, since a grab
armed for labels that no longer exist would eat the next real keystroke."
  (dolist (ov *avy-overlays*) (ignore-errors (delete-overlay ov)))
  (setf *avy-overlays* nil
        *avy-state* nil)
  (grab-key)
  nil)

;;; ---------------------------------------------------------------------------
;;; The two keystrokes

(defun %avy-key-char (key)
  "The literal character the key token KEY names, or NIL.

KEY is spelled the way `key-bindings' spells one, so everything that is not a
single character — `<esc>', `<ret>', an arrow, any chord — answers NIL and
therefore cancels. `SPC' is the one word with a character behind it, and it is a
real thing to search for."
  (cond ((string= key "SPC") #\Space)
        ((= (length key) 1) (char key 0))))

(defun %avy-show (char)
  "Label every CHAR on screen and wait for one to be pressed."
  (let* ((region (%avy-region))
         (hits (%avy-candidates char (car region) (cdr region)))
         (n (min (length hits) (length *avy-keys*))))
    (cond
      ((zerop n)
       (avy-cancel)
       (message (format nil "avy: no ~a on screen"
                        (if (char= char #\Space) "space" char))))
      (t
       (loop for pos in hits
             for i from 0 below n
             do (let ((ov (%avy-label pos (string (char *avy-keys* i)))))
                  (when ov (push ov *avy-overlays*))))
       (setf *avy-state* :label)
       (grab-key 'avy-pick)
       ;; The overflow is said rather than hidden: with more hits than letters
       ;; the ones past the cap are unlabelled and unreachable, and a jump that
       ;; silently cannot reach half the screen is worse than one that says so.
       (message (format nil "avy: ~a target~:p~@[, ~a unlabelled~]" n
                        (when (> (length hits) n) (- (length hits) n))))))))

(defun %avy-jump (key)
  "Land on the label KEY names, or cancel. The overlay is asked where it *is*
rather than a stored offset being trusted: it is a live range that moved with any
edit since, and reading it is one query."
  (let ((target (loop for ov in *avy-overlays*
                      when (string= key (overlay-get ov 'avy-label))
                        return (overlay-start ov))))
    (avy-cancel)                        ; before the jump, so the screen is clean
    (when target (goto-char target))))  ; whatever happens next

(defun avy-pick (key)
  "Receive one keystroke from `grab-key'. Not a command — the editor calls it.

Both turns arrive here, which is why `*avy-state*' exists: core knows only that
somebody wanted a key, and *which* key this is is entirely this file's business.
The T arm is the one that matters — a key arriving with no avy in progress is a
bug somewhere, and clearing is the only safe thing to do with it."
  (case *avy-state*
    (:char (let ((char (%avy-key-char key)))
             (if char (%avy-show char) (avy-cancel))))
    (:label (%avy-jump key))
    (t (avy-cancel))))

(defun avy-goto-char ()
  "Jump to a character you can see: type it, then press its label.

Bound in the config rather than here — see `runtime/init.lisp'. Deliberately not
on `/': `/' is search, in the editor and in every vim anyone brings muscle memory
from, and taking it would mean rehoming search to buy a key avy does not need."
  (avy-cancel)                          ; a second invocation replaces the first
  (setf *avy-state* :char)
  (grab-key 'avy-pick)
  (message "avy: char?"))

;;; ponytail: nothing here is on `*after-change-functions*', and an edit arriving
;;; while the labels are up — an LSP formatting reply, another mode's timer —
;;; slides them under the eye that already chose one. No keystroke can cause it,
;;; because core has handed the keyboard to this file for exactly that window,
;;; and the labels are overlays so the damage is a jump one character out rather
;;; than a stale offset. The fix, if it ever bites, is two lines:
;;;
;;;   (defun %avy-invalidate () (when *avy-state* (avy-cancel)))
;;;   (add-hook '*after-change-functions* '%avy-invalidate)
;;;
;;; ...declined for now because it puts a hook on every edit in the editor,
;;; forever, to guard a window measured in one keystroke.
