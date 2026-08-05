;;;; math.lisp — an entire maths curriculum as one org file.
;;;;
;;;; The flagship idea, and the first thing to say about it is what it is *not*:
;;;; there is no new file type here, no parallel syntax, and nothing the rest of
;;;; the editor has to be taught. A curriculum is plain org — so `org-modern'
;;;; draws its headings, `org-fold' folds its units, `org-latex-preview'
;;;; typesets its equations and `org-inline-images' draws its figures, all of
;;;; them unmodified and none of them aware that any of this exists. Everything
;;;; below is *schema*: which properties mean what, and how to find them.
;;;;
;;;; That decision is worth defending because the obvious alternative is worse.
;;;; A `.curriculum' file with its own parser would mean a second syntax
;;;; highlighter, a second folding rule, a second LaTeX pass and a second
;;;; renderer, and the moment a lesson wanted a table or a footnote it would mean
;;;; reimplementing org. Writing the schema as *properties on ordinary headings*
;;;; costs one reader per property and buys the whole of org for free — and it
;;;; means a curriculum opened in Emacs, or on GitHub, is still a document you
;;;; can read.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; THE CONTRACT
;;;;
;;;; Two more parts of this feature are built against the names below and they
;;;; are load-bearing. `docs/curriculum.org' is the same contract written for
;;;; whoever *generates* a curriculum; this is it written for whoever reads one.
;;;;
;;;;   (math-curriculum-p)          T when the live buffer declares itself one
;;;;   (math-units)                 (:id :title :begin :end), document order
;;;;   (math-unit-at-point)         one of the above, or NIL
;;;;   (math-problems &optional unit)   problem plists, document order
;;;;   (math-problem-at-point)      one problem plist, or NIL
;;;;   (math-set-status problem status) rewrite `:ZEMACS_STATUS:'
;;;;   (math-response-insert problem text)  replace the Response body
;;;;   (math-src-block problem)     (:lang :tangle :body :begin :end), or NIL
;;;;   (math-problem-tag problem)   the scene `:tag' a click on it comes back as
;;;;
;;;; A problem plist:
;;;;
;;;;   :kind            "written" | "programming"
;;;;   :status          "todo" | "done"
;;;;   :id              the owning unit's :ID:, or NIL outside a unit
;;;;   :packages        list of strings, possibly empty
;;;;   :begin :end      character offsets of the whole problem subtree
;;;;   :response-begin  character offsets of the *body* of its `Response'
;;;;   :response-end    subsection, or both NIL when it has none
;;;;
;;;; Every offset in this file is a **character** offset, which is what every
;;;; primitive in this editor takes. `buffer-string' hands out UTF-8 bytes and
;;;; `%org-lines' does the conversion once per line; nothing above that layer
;;;; ever sees a byte.
;;;;
;;;; The same is true of the *text* in a plist, and it has to be said separately
;;;; because it is a different conversion: `:title' and every property value go
;;;; through `utf8-text' as they are read. Offsets and text were decoded in
;;;; different places for a while, and the symptom was a status line reading
;;;; `Ãlgebra â€" Vectores' under a page whose cursor landed exactly right. The
;;;; readers below are therefore the boundary: a caller may `message' a title or
;;;; put a status in a `run' without asking where it came from.
;;;;
;;;; The line *vector* is still raw, deliberately — the writers re-find their
;;;; heading in it by offset, and `%org-property' matches ASCII markers in it —
;;;; so a `first' out of `%org-lines' is bytes and everything a plist carries is
;;;; characters.
;;;;
;;;; Offsets go stale. A plist is a *reading*, not a handle: it describes the
;;;; buffer as it was when it was made, and a keystroke that lands afterwards
;;;; moves everything after it. So the two writers below re-read before they
;;;; write and use the plist only to say *which* problem was meant. A caller that
;;;; holds a plist across an edit and then reads `:begin' from it will be a
;;;; character or two out; a caller that hands it straight back to
;;;; `math-set-status' will not.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; STATUS IS THE USER'S
;;;;
;;;; `:ZEMACS_STATUS:' is `todo' or `done' and **nothing in this system decides
;;;; it**. The user verifies their own solutions, with a CAS or a proof assistant
;;;; or their own eyes, and this records what they say. `math-set-status' writes
;;;; whatever it is handed and has no opinion; there is no checker, no partial
;;;; credit and no "correct". The one automatic write that will exist — the
;;;; capture of a photographed solution — sets `done' to mean *captured*, and the
;;;; user is free to set it straight back.
;;;;
;;;; That is a deliberate refusal, not a gap. An editor that marked a proof wrong
;;;; would be wrong about a proof, and the failure mode of being told you are
;;;; wrong when you are right is that you stop trusting the tool.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; ONE SCAN, AND WHY THE WRITERS ARE ONE CALL
;;;;
;;;; Every reader here starts from `%org-lines' — one `buffer-string' and a walk
;;;; — rather than from `line-string' per line. On a document the size of a
;;;; textbook the difference is one round trip through `%query' against several
;;;; thousand, and it is the reason `math-problems' is affordable on a keystroke.
;;;;
;;;; Both writers are a single `replace-region'. `init.lisp' says why at the top
;;;; of the file: Lisp here runs on its own thread and is not atomic with respect
;;;; to the user, so a keystroke can land *between* two primitives. A
;;;; `delete-region' followed by an `insert' can therefore be interrupted, and a
;;;; half-written property drawer is a corrupted document rather than a wrong
;;;; status. Everything either writer needs is computed first and applied in one
;;;; call, which is also one undo step.
;;;;
;;;; ---------------------------------------------------------------------------
;;;; ponytail: there is no index and no cache. Every reader rescans the buffer,
;;;; which is a `buffer-string' plus a pass over its lines — a millisecond or so
;;;; on a curriculum, and correct by construction, since nothing can go stale if
;;;; nothing is remembered. The ceiling is a document big enough that a scan per
;;;; keystroke is felt, and the upgrade path is the change *delta* `boundary.org'
;;;; lists as missing: with it, an index could be maintained instead of rebuilt.
;;;;
;;;; ponytail: a problem's `Response' is found by its heading *text*, not by a
;;;; property. That means a curriculum in another language, or one that spells it
;;;; `Solution', needs `*math-response-heading*' set. A `:ZEMACS_RESPONSE: t'
;;;; property would be language-proof and is one more thing for a generator to
;;;; get right; the heading is what a human reads and what a human will type.

(in-package :zemacs)

;;; ---------------------------------------------------------------------------
;;; The vocabulary
;;;
;;; Every literal this file matches on, in one place, so a curriculum in another
;;; language is a config change rather than a patch. DEFPARAMETER throughout:
;;; reloading the config should put the shipped spelling back, since a stale
;;; override is indistinguishable from a file that does not parse.

(defparameter *math-curriculum-keyword* "ZEMACS_CURRICULUM"
  "The `#+' keyword that declares a file a curriculum. Its *value* is a format
version and is deliberately not checked: there is one version, and refusing to
open version 2 in an editor that could have read it is a worse failure than
whatever version 2 changes.")

(defparameter *math-problem-property* "ZEMACS_PROBLEM"
  "The property that makes a heading a problem. Its value is the kind.")

(defparameter *math-programming-kind* "programming"
  "The `:ZEMACS_PROBLEM:' value meaning a problem is answered with a program
rather than in prose.

The only place anything here branches on the *kind*. `math-code.lisp' asks
whether the problem carries a source block instead, which is the question a
runner has to ask; a page has to choose which buttons to draw before it has read
one, and the schema already says which sort of problem this is.")

(defparameter *math-status-property* "ZEMACS_STATUS"
  "The property that carries `todo' or `done'.")

(defparameter *math-packages-property* "ZEMACS_PACKAGES"
  "The property listing the Python packages a programming problem needs.")

(defparameter *math-id-property* "ID"
  "The property that makes a top-level heading a unit, and the target an
`[[id:...]]' link in the Contents section resolves against. Org's own, so a
curriculum's ids work in Emacs unchanged.")

(defparameter *math-response-heading* "Response"
  "The heading a problem's answer lives under. Matched case-insensitively.")

(defparameter *math-contents-heading* "Contents"
  "The heading `math-goto-contents' jumps back to.")

(defparameter *math-todo* "todo")
(defparameter *math-done* "done")

;;; ---------------------------------------------------------------------------
;;; Reading the outline
;;;
;;; Three small text helpers and then the one function everything is built on.
;;; `%org-lines', `%org-line-level' and `%org-property' come from
;;; `org-modern.lisp', which is loaded first: this file deliberately owns no
;;; parser of its own, because a second reading of what a heading is would drift
;;; from the one that draws them.

(defun %math-trim (s)
  "S without leading or trailing whitespace. Written out once because a drawer
line is indented, `:ID:  x' happens as often as `:ID: x', and a reader that
strips its own whitespace slightly differently from the next one is a bug you
find in a generated file six months later."
  (string-trim '(#\Space #\Tab #\Return) s))

(defun %math-indent (s)
  "S's leading whitespace, so a rewritten line keeps the column it was in."
  (subseq s 0 (or (position-if-not #'%org-blank-p s) (length s))))

(defun %math-blank-line-p (s)
  (string= "" (%math-trim s)))

(defun %math-words (s)
  "S split on whitespace and commas, empties dropped; NIL for NIL.

Both separators, because `numpy sympy' and `numpy, sympy' are both what somebody
writes and neither is worth being right about."
  (when s
    (let ((out nil) (start nil) (n (length s)))
      (dotimes (i n)
        (let ((sep (or (%org-blank-p (char s i)) (char= (char s i) #\,))))
          (cond ((and sep start) (push (subseq s start i) out) (setf start nil))
                ((and (not sep) (null start)) (setf start i)))))
      (when start (push (subseq s start) out))
      (nreverse out))))

(defun %math-keyword (name &optional (lines (%org-lines)))
  "The value of the `#+NAME:' keyword line, or NIL.

Searched in the **preamble** only — the lines above the first heading — and that
is not an optimisation. It is where org puts keywords and where this format
specifies them, and it is what stops a `#+ZEMACS_CURRICULUM' quoted inside a
source block, in a document *about* the format, from turning that document into
a curriculum. `docs/curriculum.org' is exactly such a document.

The *value* is decoded and the matching is not. A keyword's name is ASCII by the
format, so the prefix compare can run against the raw bytes and the tail after it
still starts on a character boundary; the value is free text a person wrote —
`#+TITLE:' most of all — and `math-progress' puts it straight in the status
line, where undecoded bytes come out as mojibake."
  (let ((want (concatenate 'string "#+" name ":")))
    (dolist (l lines)
      (let ((text (first l)))
        (when (%org-line-level text) (return nil))
        (let ((s (%math-trim text)))
          (when (and (>= (length s) (length want))
                     (string-equal want s :end2 (length want)))
            (return (utf8-text
                     (string-trim '(#\Space #\Tab)
                                  (subseq s (length want)))))))))))

(defun %math-drawer-lines (v n i)
  "(OPEN . CLOSE) line indices of the property drawer belonging to the heading on
line I of the line vector V, or NIL when it has none.

OPEN is its `:PROPERTIES:' and CLOSE its `:END:'. Blank lines between the
heading and the drawer are skipped, which org itself is stricter about — a
deliberate leniency, because this format is *generated* and one stray newline
should cost a blank line rather than silently turn a problem back into prose.

A heading found before the `:END:' means the drawer was never closed, and the
answer is NIL rather than a drawer that swallows the rest of the document."
  (let ((j (1+ i)))
    (loop while (and (< j n) (%math-blank-line-p (first (aref v j))))
          do (incf j))
    (when (and (< j n) (string-equal ":PROPERTIES:" (%math-trim (first (aref v j)))))
      (let ((close (loop for k from (1+ j) below n
                         for text = (first (aref v k))
                         do (cond ((string-equal ":END:" (%math-trim text)) (return k))
                                  ((%org-line-level text) (return nil))))))
        (when close (cons j close))))))

(defun %math-properties (v n i)
  "((NAME . VALUE) ...) for the heading on line I, names upcased.

An alist and not a plist: the names come out of a file and are therefore
strings, and `assoc' with `string-equal' is the case-insensitive lookup org
promises. Interning them as keywords would put every typo in a generated file
into the image's symbol table forever.

The whole drawer line is decoded before it is split, rather than the value
afterwards. One call instead of one per property, and it leaves the name and the
value in the same index space as the `position' that separated them — a value
decoded after the split would be right and a name decoded nowhere would be a
second rule to remember. Values are why it matters: a property is free text a
generator wrote, and `%math-page-problem' puts one straight in a `run'."
  (let ((d (%math-drawer-lines v n i)))
    (when d
      (loop for j from (1+ (car d)) below (cdr d)
            for trimmed = (utf8-text (%math-trim (first (aref v j))))
            for colon = (and (> (length trimmed) 1)
                             (char= (char trimmed 0) #\:)
                             (position #\: trimmed :start 1))
            when colon
              collect (cons (string-upcase (subseq trimmed 1 colon))
                            (string-trim '(#\Space #\Tab) (subseq trimmed (1+ colon))))))))

(defun %math-property-line (v drawer name)
  "Line index of the `:NAME:' line inside DRAWER, or NIL."
  (loop for j from (1+ (car drawer)) below (cdr drawer)
        when (%org-property (first (aref v j)) name) return j))

(defun %math-headings (&optional (lines (%org-lines)))
  "Every heading in LINES, in document order, as a plist:

    :level     stars
    :title     the text after them, trimmed
    :begin     character offset of the first star
    :head-end  character offset of the end of the heading *line*
    :end       character offset of the end of the last line of its subtree
    :props     ((NAME . VALUE) ...)
    :index     its line index, which the writers below need and readers ignore

`:end' cannot be known going forward — a subtree runs until a heading of the
same or shallower level, which is org's own rule and the one `org-fold' folds
by — so it is resolved from the heading list rather than by a search per
heading. That is one pass over the lines and one over the headings, instead of
the quadratic walk the obvious version does on a flat document."
  (let* ((v (coerce lines 'vector))
         (n (length v))
         (raw nil))
    (dotimes (i n)
      (let ((level (%org-line-level (first (aref v i)))))
        (when level (push (cons i level) raw))))
    (setf raw (nreverse raw))
    (loop for (entry . rest) on raw
          collect (let* ((i (car entry))
                         (level (cdr entry))
                         (line (aref v i))
                         (next (find-if (lambda (e) (<= (cdr e) level)) rest))
                         (last (max i (if next (1- (car next)) (1- n)))))
                    (list :level level
                          ;; Decoded, like every other piece of *text* in a
                          ;; plist here: a title is prose and `math-units'
                          ;; hands it to whatever draws a curriculum. LEVEL is
                          ;; a count of leading stars, so the SUBSEQ still
                          ;; starts on a character boundary.
                          :title (utf8-text
                                  (%math-trim (subseq (first line) level)))
                          :begin (second line)
                          :head-end (third line)
                          :end (third (aref v last))
                          :props (%math-properties v n i)
                          :index i)))))

(defun %math-prop (heading name)
  "HEADING's NAME property, or NIL. Case-insensitive, as org is."
  (cdr (assoc name (getf heading :props) :test #'string-equal)))

(defun %math-limit (lines)
  "`point-max', read off LINES rather than asked for — one round trip saved per
reader, and the same answer: the end of the last line is the end of the buffer."
  (let ((last (car (last lines))))
    (if last (third last) 0)))

;;; ---------------------------------------------------------------------------
;;; The schema — units
;;;
;;; A **unit is a top-level heading carrying an `:ID:'**, and that rule is doing
;;; two jobs. It is what excludes `* Contents' without this file having to know
;;; the word "Contents", and it is what makes "every unit has an id" a fact the
;;; format guarantees rather than a convention a generator might forget: a
;;; heading without one is simply not a unit, so a Contents link that resolves to
;;; nothing is impossible to write without also losing the unit.

(defun math-curriculum-p (&optional (lines (%org-lines)))
  "T when the live buffer declares itself a curriculum with
`#+ZEMACS_CURRICULUM'."
  (and (%math-keyword *math-curriculum-keyword* lines) t))

(defun math-units (&optional (headings (%math-headings)))
  "Every unit, in document order, as (:id :title :begin :end).

:BEGIN is the first star of the heading and :END the end of the last line of its
subtree, so `(buffer-substring :begin :end)' is the unit whole — instruction,
problems and all."
  (let ((out nil))
    (dolist (h headings (nreverse out))
      (let ((id (%math-prop h *math-id-property*)))
        (when (and (= 1 (getf h :level)) id (plusp (length id)))
          (push (list :id id
                      :title (getf h :title)
                      :begin (getf h :begin)
                      :end (getf h :end))
                out))))))

(defun math-unit-at-point (&optional (units (math-units)))
  "The unit point is inside, or NIL — which is the honest answer in the Contents
section and in the preamble above it."
  (let ((at (point)))
    (find-if (lambda (u) (and (<= (getf u :begin) at) (<= at (getf u :end)))) units)))

;;; ---------------------------------------------------------------------------
;;; The schema — problems
;;;
;;; A **problem is any heading carrying `:ZEMACS_PROBLEM:'**. Any heading, at any
;;; depth: the format writes them as `** Problem' under a unit because that is
;;; what reads well, but nothing here depends on the level, so a unit may grow
;;; sections and put its problems under those without a line changing here.

(defun %math-response-heading (problem headings)
  "PROBLEM's `Response' subsection heading, or NIL.

Its own subsection and not a deeper problem's: the search is bounded by the
problem's own subtree, which is what stops a problem written without a Response
from picking up the next problem's."
  (let ((level (getf problem :level))
        (beg (getf problem :begin))
        (end (getf problem :end)))
    (find-if (lambda (h)
               (and (> (getf h :level) level)
                    (> (getf h :begin) beg)
                    (<= (getf h :end) end)
                    (string-equal (getf h :title) *math-response-heading*)))
             headings)))

(defun %math-response-span (problem headings v limit)
  "(BEGIN . END) of the *body* of PROBLEM's `Response' subsection, or NIL.

The body, not the section: BEGIN is the start of the line after the `Response'
heading and END the end of its last **non-blank** line, newline included. Both
halves of that matter and both are about `math-response-insert' being
idempotent.

Excluding the heading is what lets the body be replaced without the section
having to be rewritten. Stopping at the last non-blank line is what leaves the
blank line before the next heading alone — included, the first rewrite would
swallow it and the second would not, so writing the same transcription twice
would change the document once. An empty Response answers BEGIN = END, which is
an insertion point and needs no separate case anywhere."
  (let ((r (%math-response-heading problem headings)))
    (when r
      (let* ((head (getf r :index))
             (last (or (position (getf r :end) v :key #'third) head))
             (body-last (loop for j from last downto (1+ head)
                              unless (%math-blank-line-p (first (aref v j)))
                                return j
                              finally (return head)))
             (begin (min limit (1+ (getf r :head-end)))))
        (cons begin
              (if (> body-last head)
                  (max begin (min limit (1+ (third (aref v body-last)))))
                  begin))))))

(defun %math-problem (heading unit-id headings v limit)
  "HEADING as a problem plist. See the contract at the top of this file."
  (let ((kind (%math-prop heading *math-problem-property*))
        (status (%math-prop heading *math-status-property*))
        (span (%math-response-span heading headings v limit)))
    (list :kind (string-downcase kind)
          :status (if (and status (plusp (length status)))
                      (string-downcase status)
                      *math-todo*)
          :id unit-id
          :packages (%math-words (%math-prop heading *math-packages-property*))
          :begin (getf heading :begin)
          :end (getf heading :end)
          :response-begin (car span)
          :response-end (cdr span))))

(defun math-problems (&optional unit (lines (%org-lines)))
  "Every problem in the buffer, or in UNIT alone, in document order.

UNIT is a plist from `math-units'; passing one is a filter and nothing more, so
`(math-problems (math-unit-at-point))' is the whole of \"this unit's problems\"."
  (let* ((v (coerce lines 'vector))
         (headings (%math-headings lines))
         (limit (%math-limit lines))
         (lo (and unit (getf unit :begin)))
         (hi (and unit (getf unit :end)))
         (unit-id nil)
         (out nil))
    (dolist (h headings (nreverse out))
      ;; The owning unit is tracked forwards rather than searched for per
      ;; problem: a problem belongs to the last top-level heading above it, which
      ;; is one variable and no second pass.
      (when (= 1 (getf h :level))
        (setf unit-id (%math-prop h *math-id-property*)))
      (let ((kind (%math-prop h *math-problem-property*)))
        (when (and kind (plusp (length kind))
                   (or (null unit)
                       (and (>= (getf h :begin) lo) (<= (getf h :end) hi))))
          (push (%math-problem h unit-id headings v limit) out))))))

(defun math-problem-at-point ()
  "The problem point is inside, or NIL.

Problems do not nest, so \"inside\" is unambiguous and the first match is the
only one. Point on the `** Problem' heading itself counts, which is where
`math-next-problem' leaves you."
  (let ((at (point)))
    (find-if (lambda (p) (and (<= (getf p :begin) at) (<= at (getf p :end))))
             (math-problems))))

;;; ---------------------------------------------------------------------------
;;; The schema — a programming problem's source block
;;;
;;; One block per problem, because a problem is one thing to implement. The
;;; format says so and this reads the first; a second block in the same problem
;;; is prose as far as everything here is concerned.

(defun %math-src-header (text)
  "(LANG . WORDS) of a `#+begin_src lang :k v ...' line, or NIL.

WORDS is everything after the language, which is what `%math-src-arg' picks
`:tangle' out of. Org calls these header arguments and has a dozen; this reads
the one that decides a filename and leaves the rest in the buffer, where the
file is the source of truth."
  (let* ((s (%math-trim text))
         (tag "#+begin_src"))
    (when (and (>= (length s) (length tag))
               (string-equal tag s :end2 (length tag)))
      (let ((words (%math-words (subseq s (length tag)))))
        (cons (first words) (rest words))))))

(defun %math-src-arg (words name)
  "The value following NAME in WORDS, or NIL.

A header argument whose value is itself another argument — `:tangle :results' —
is a malformed header, and answering NIL for it is better than answering
`:results'."
  (let ((tail (member name words :test #'string-equal)))
    (when (and tail (rest tail))
      (let ((v (second tail)))
        (when (or (zerop (length v)) (char/= (char v 0) #\:)) v)))))

(defun math-src-block (problem &optional (lines (%org-lines)))
  "PROBLEM's source block as (:lang :tangle :body :begin :end), or NIL.

:BEGIN and :END span the **body** — the lines between `#+begin_src' and
`#+end_src', newline included — and not the block. That is what a runner wants:
`(buffer-substring :begin :end)' is exactly :BODY, and replacing that range
rewrites the program without disturbing the directives around it. An empty block
answers :BEGIN = :END, which is where the first line of a program goes.

:TANGLE is the `:tangle' header argument verbatim, so org's `:tangle no' arrives
as the string \"no\" rather than as NIL — the difference between \"this block
has no filename\" and \"this block says do not write one\" is the caller's to
make, not this reader's."
  (let* ((v (coerce lines 'vector))
         (n (length v))
         (beg (getf problem :begin))
         (end (getf problem :end)))
    (loop for i from 0 below n
          for line = (aref v i)
          when (and (>= (second line) beg) (<= (third line) end))
            do (let ((header (%math-src-header (first line))))
                 (when header
                   (let ((close (loop for j from (1+ i) below n
                                      for text = (first (aref v j))
                                      do (cond ((%org-directive-p text "#+end_src")
                                                (return j))
                                               ((> (third (aref v j)) end) (return nil))))))
                     (return
                       (when close
                         (let ((begin (min (1+ (third line)) (second (aref v close))))
                               (end (second (aref v close))))
                           (list :lang (car header)
                                 :tangle (%math-src-arg (cdr header) ":tangle")
                                 ;; Rebuilt from the lines rather than asked for
                                 ;; with `buffer-substring': the text is already
                                 ;; in hand, and a second query could answer about
                                 ;; a buffer a keystroke has since changed — so
                                 ;; the body and the offsets would disagree.
                                 :body (with-output-to-string (s)
                                         (loop for j from (1+ i) below close
                                               do (write-string (first (aref v j)) s)
                                                  (write-char #\Newline s)))
                                 :begin begin
                                 :end end))))))))))

;;; ---------------------------------------------------------------------------
;;; The two writers
;;;
;;; Each is a *read*, a decision, and exactly one `replace-region'. See the note
;;; at the top of this file for why that is not negotiable.

(defun %math-heading-line (v at)
  "Index of the heading line beginning at character offset AT, or NIL.

The re-find both writers start with. A plist is a reading of the buffer as it
was; this is the check that it still describes the buffer as it is, and it is
cheap enough to do unconditionally."
  (let ((i (position at v :key #'second)))
    (when (and i (%org-line-level (first (aref v i)))) i)))

(defun math-set-status (problem status)
  "Set PROBLEM's `:ZEMACS_STATUS:' to STATUS, in one edit. Answers the status
written, or NIL when PROBLEM is no longer where its plist says it is.

STATUS may be a string or a symbol and is written lower-cased. The format says
`todo' or `done'; this does not enforce it, and that is deliberate — see the
refusal at the top of this file. Recording what the user says is the whole job.

Three cases, all one call: the property line is there and is rewritten whole;
there is a drawer without it, and one line goes into it; there is no drawer, and
the whole thing goes under the heading. Indentation is carried across so a
problem nested inside a list keeps its column.

`with-inhibited-read-only', because a curriculum being *read* is frozen.
`org-frozen-mode' claims the buffer read-only and `Editor::apply' then refuses
every write into it — including this one, from Lisp, with nothing said. The lift
goes here rather than at the call sites because every caller wants it and none of
them can be expected to remember: a click on a status pill, a transcription
landing from the photograph watcher, and `SPC m t' are one edit reached three
ways. Lifting a claim that is not there is a `buffer-read-only-p' and nothing
else, so an ordinary org buffer pays a reader and no behaviour."
  (let* ((want (string-downcase (string status)))
         (v (coerce (%org-lines) 'vector))
         (n (length v))
         (i (%math-heading-line v (getf problem :begin))))
    (cond
      ((null i) (message "math: that problem has moved — read it again") nil)
      (t (let* ((drawer (%math-drawer-lines v n i))
                (line (and drawer (%math-property-line v drawer *math-status-property*))))
           (with-inhibited-read-only
             (cond
               (line (let ((l (aref v line)))
                       (replace-region (second l) (third l)
                                       (format nil "~a:~a: ~a" (%math-indent (first l))
                                               *math-status-property* want))))
               (drawer (let ((open (aref v (car drawer))))
                         (replace-region (third open) (third open)
                                         (format nil "~%~a:~a: ~a"
                                                 (%math-indent (first open))
                                                 *math-status-property* want))))
               (t (let ((head (aref v i)))
                    (replace-region (third head) (third head)
                                    (format nil "~%~a:PROPERTIES:~%~a:~a: ~a~%~a:END:"
                                            (%math-indent (first head))
                                            (%math-indent (first head))
                                            *math-status-property* want
                                            (%math-indent (first head))))))))
           want)))))

(defun %math-response-text (text)
  "TEXT as a Response body: trailing whitespace gone, exactly one newline at the
end, and the empty string for nothing at all.

The normalisation is what makes writing the same transcription twice a no-op.
Without it, a model that ends its answer with a newline and one that does not
would produce two different documents, and `math-response-insert' would grow a
blank line per call."
  (let ((s (string-right-trim '(#\Space #\Tab #\Return #\Newline) (string text))))
    (if (zerop (length s)) "" (concatenate 'string s (string #\Newline)))))

(defun math-response-insert (problem text)
  "Replace the body of PROBLEM's `Response' subsection with TEXT, in one edit.
Answers T, or NIL when PROBLEM is no longer where its plist says it is.

Creates the subsection when there is none, one level deeper than the problem and
under its last line of prose — so a problem written without a `*** Response'
still has somewhere for an answer to land, and a generator is not obliged to
emit an empty heading it cannot know will be used.

Idempotent: calling it twice with the same TEXT leaves the buffer identical the
second time. That is a property of `%math-response-span' stopping at the last
non-blank line and of `%math-response-text' normalising the trailing newline,
and it is the property that matters most here — the caller that will use this is
a capture pass that may well run twice on the same photograph.

`with-inhibited-read-only' for the reason `math-set-status' gives at length: the
buffer a transcription lands in is very often one `org-frozen-mode' has frozen,
and a write refused by `Editor::apply' is a write that says nothing."
  (let* ((body (%math-response-text text))
         (lines (%org-lines))
         (v (coerce lines 'vector))
         (i (%math-heading-line v (getf problem :begin))))
    (cond
      ((null i) (message "math: that problem has moved — read it again") nil)
      (t
       (let* ((headings (%math-headings lines))
              (h (find (getf problem :begin) headings
                       :key (lambda (x) (getf x :begin))))
              (span (and h (%math-response-span h headings v (%math-limit lines)))))
         (with-inhibited-read-only
           (cond
             (span (replace-region (car span) (cdr span) body))
             (t
              ;; No Response section. Put one under the last line of the problem
              ;; that has anything on it, which keeps the blank lines a document
              ;; already has between problems where they were.
              (let* ((last (or (position (getf h :end) v :key #'third) i))
                     (at (loop for j from last downto i
                               unless (%math-blank-line-p (first (aref v j)))
                                 return j
                               finally (return i)))
                     (stars (make-string (1+ (getf h :level)) :initial-element #\*)))
                (replace-region (third (aref v at)) (third (aref v at))
                                (format nil "~%~%~a ~a~%~a"
                                        stars *math-response-heading* body))))))
         t)))))

;;; ---------------------------------------------------------------------------
;;; The motions
;;;
;;; What the keyboard actually wears out: step to the next problem, step back,
;;; say whether this one is done, and get back to the Contents. Everything else
;;; a curriculum needs is org's already — `RET' on a Contents link is
;;; `org-open-at-point', folding a unit is `z a', and `SPC m l' typesets the
;;; equations.

(defun %math-describe (problem)
  "A one-line description of PROBLEM for the status line."
  (format nil "~a · ~a~@[ · ~{~a~^ ~}~]"
          (getf problem :kind) (getf problem :status) (getf problem :packages)))

(defun math-next-problem ()
  "Move to the next problem heading."
  (let* ((at (point))
         (next (find-if (lambda (p) (> (getf p :begin) at)) (math-problems))))
    (cond (next (goto-char (getf next :begin)) (message (%math-describe next)))
          (t (message "no problem after this one")))))

(defun math-previous-problem ()
  "Move to the previous problem heading."
  (let* ((at (point))
         (prev (find-if (lambda (p) (< (getf p :begin) at))
                        (reverse (math-problems)))))
    (cond (prev (goto-char (getf prev :begin)) (message (%math-describe prev)))
          (t (message "no problem before this one")))))

(defun math-toggle-status ()
  "Flip the problem at point between `todo' and `done'.

The only thing in this editor that writes a status, and it is bound to a key
because the user is the only thing that decides one."
  (let ((p (math-problem-at-point)))
    (cond ((null p) (message "no problem at point"))
          (t (let ((want (if (string= (getf p :status) *math-done*)
                             *math-todo* *math-done*)))
               (math-set-status p want)
               (message (format nil "~a: ~a" (getf p :kind) want)))))))

(defun math-goto-contents ()
  "Jump back to the Contents section."
  (let ((at (%org-heading-position *math-contents-heading*)))
    (if at (goto-char at) (message "no Contents heading"))))

(defun math-progress ()
  "Say how much of the curriculum is done.

One line, and it is the line a learner wants on opening the file: what this is,
and how far in they are."
  (let* ((lines (%org-lines))
         (ps (math-problems nil lines))
         (done (count *math-done* ps :key (lambda (p) (getf p :status))
                                     :test #'string=)))
    (message (format nil "~@[~a — ~]~d/~d problem~:p done"
                     (%math-keyword "TITLE" lines) done (length ps)))))

;;; ---------------------------------------------------------------------------
;;; The minor mode
;;;
;;; A *minor* mode and not a major one, and that is the whole design in one
;;; decision: a curriculum is an org buffer, so it must keep org's major mode and
;;; everything hanging off it. A `math-mode' deriving from `org-mode' would work
;;; and would be wrong — it would mean every org feature added afterwards had to
;;; remember to derive too, and `org-modern.lisp' re-declares `org-mode'
;;; deliberately, so the last declaration in the runtime wins and a second one
;;; here would silently replace it.
;;;
;;; Instead the mode is turned on from `*org-mode-functions*', the hook list
;;; `org-modern.lisp' runs at the end of the org-mode body — the same PUSHNEW
;;; shape as `*after-change-functions*' and `*point-moved-functions*', and
;;; idempotent across a config reload for the same reason.
;;;
;;; What it buys is a keymap that exists only in a curriculum: a minor mode's
;;; bindings are consulted before the major mode's, so `SPC m n' means "next
;;; problem" in a curriculum and is free in every other org buffer.

(define-minor-mode math-curriculum
  "Curriculum motions for a `#+ZEMACS_CURRICULUM' org file: `SPC m n' and
`SPC m p' step between problems, `SPC m t' marks one done, `SPC m g' goes back
to the Contents and `SPC m s' says how far in you are.")

(defun math-curriculum-maybe ()
  "Turn `math-curriculum' on in a buffer that declares itself one.

On `*org-mode-functions*', so opening a curriculum *is* the whole installation.
Guarded by `minor-mode-p' because the org-mode body runs on every entry into the
mode and the mode command is a toggle — without it, re-entering org-mode would
switch the curriculum motions off."
  (when (and (math-curriculum-p) (not (minor-mode-p 'math-curriculum)))
    (math-curriculum)
    ;; Last, so its line is the one left on screen: `%toggle-minor-mode' ends by
    ;; announcing the mode, and the progress line is what is actually worth
    ;; reading.
    (math-progress)))

;;; DEFVAR before the PUSHNEW, exactly as `org-modern.lisp' does for the other
;;; two hook lists: a build whose `*runtime-dir*' never found that file must get
;;; an empty list here rather than an unbound variable.
(defvar *org-mode-functions* nil)
(pushnew 'math-curriculum-maybe *org-mode-functions*)

(define-key "math-curriculum" "SPC m n" "math-next-problem")
(define-key "math-curriculum" "SPC m p" "math-previous-problem")
(define-key "math-curriculum" "SPC m t" "math-toggle-status")
(define-key "math-curriculum" "SPC m g" "math-goto-contents")
(define-key "math-curriculum" "SPC m s" "math-progress")

;;; The readers are nouns and `M-x' is a list of verbs. `*hidden-commands*' is
;;; `init.lisp''s existing answer to exactly this — `%zero-arg-p' counts a
;;; function whose arguments are all `&optional' as callable, which every reader
;;; above is — so they are declared here rather than by renaming them with a `%'
;;; they do not deserve: these are the published contract.
(when (boundp '*hidden-commands*)
  (dolist (name '("math-curriculum-p" "math-units" "math-unit-at-point"
                  "math-problems" "math-problem-at-point"))
    (pushnew name *hidden-commands* :test #'string=)))

;;; ---------------------------------------------------------------------------
;;; THE PAGE — a curriculum that shows its own state
;;;
;;; `org-frozen-mode' draws an org file as a printed page, and the premise of a
;;; printed page is that it shows no machinery. `:ZEMACS_STATUS:' *is* machinery:
;;; it lives in a property drawer, `%org-frozen-scan' classifies every drawer
;;; line `:HIDDEN', and no node is ever built for one. So the one mode in this
;;; editor built for *reading* a curriculum was the one place a curriculum's
;;; state could not be seen at all — `math-progress' would put `3/7 problems
;;; done' in the status line and nothing on the page said which three, and
;;; `:ZEMACS_PROBLEM:' went with it, so a problem was indistinguishable from any
;;; other `** Problem' heading.
;;;
;;; That was not a bug in either file. It is the correct consequence of "a
;;; printed page shows no machinery" meeting "the state is stored as machinery",
;;; and the fix is the one a book makes: the state stops being a drawer and
;;; becomes **a mark on the problem**. A `✓' beside a heading is not the
;;; property; it is what the property means.
;;;
;;; Three things reach the page, in the order they were needed:
;;;
;;;   a mark    every problem carries `✓ done' or `□ todo', and clicking it is
;;;             `SPC m t' — still the only write in this editor that sets a
;;;             status, and still the user's.
;;;   a count   every unit carries its own `2/3', which is `math-progress' said
;;;             where the reader is looking instead of in a status line they
;;;             have already scrolled past.
;;;   a way on  a programming problem gets `▶ run' and `✎ open'; a written one
;;;             gets `✎ answer', which *leaves the page*.
;;;
;;; ** The rule for every callback here: close over the problem
;;;
;;; A click is not a keystroke. `math-toggle-status', `math-code-run' and
;;; `math-code-edit' all start from `(point)', and a scene has no cursor — point
;;; is wherever the keyboard last left it, which is very likely a different
;;; problem from the one under the pointer. So every closure below carries the
;;; problem plist it was built for and none of them re-derives it. `math-set-status'
;;; and `math-response-insert' already take a problem and re-find its heading
;;; before writing, so they were correct as they stood; `math-code.lisp' grew the
;;; two siblings this needs.
;;;
;;; ** Why there is no answer box
;;;
;;; A scene is read-only and has no cursor, so there is no typing an answer into
;;; one — `docs/gui.org' lists that among its ceilings and means it. Inventing a
;;; text widget would mean an editing surface inside the thing built for not
;;; being one. The two routes that *do* work are a photograph (`SPC m r', which
;;; never needed the cursor for the text, only for the binding) and going back to
;;; the manuscript, so `✎ answer' is the second one made obvious rather than left
;;; to be remembered.
;;;
;;; ** Nothing here is a second scanner
;;;
;;; Every fact on the page comes from the schema functions above. This section
;;; adds no reading of org, no heading rule and no property name; it is a
;;; *rendering* of `math-problems' and `math-units', joined to the heading being
;;; drawn by a character offset both sides already agree on.

(defparameter *math-page-marks*
  '(("done" "✓" "string")
    ("todo" "□" "comment"))
  "(STATUS GLYPH FACE) per `:ZEMACS_STATUS:' — the mark drawn on a problem.

The glyphs are org-modern's checkbox vocabulary (`*org-modern-checkboxes*'), and
that is an argument rather than a preference: a reader of this editor already
reads a tick as done wherever a `- [X]' appears, so a problem's status needs no
legend beside it.

They are `✓' and `□' rather than the `☑' and `☐' this table shipped with, and
the reason is not taste. `SFNSMono' is the first entry of the renderer's
`FONT_CANDIDATES' on a current macOS and its `cmap' does not carry `☑', `☐' or
`☒' — so those drew *nothing at all*, and a missing glyph reads as a space,
which is why `☐ todo' looked like a stray indent rather than like a bug. Every
glyph named here is present in both `SFNSMono' and `Menlo'. Check that before
changing one: until the renderer falls back to another face for a character the
first one lacks, a bullet its font does not carry is silently invisible and
nothing anywhere says why.

A status this table does not list still draws — see `%math-page-mark'.")

(defparameter *math-page-tint* "modeline"
  "Face whose colour backs a pill or a button on the page.

Borrowed rather than earned, and `*org-frozen-code-face*' makes the same
admission at length: a scene's colours are face *names* — `Block::background' is
a number naming an entry of `face-list' — and the theme holds exactly one colour
already chosen to sit behind text at low contrast. Two costs, both real: a config
restyling its modeline restyles these with it, and a `todo' pill and a `done'
pill are the same tint told apart by their glyph and its colour. The upgrade path
is the one already written down for overlays — a primitive taking three floats,
the shape `set-syntax-color' has.")

(defparameter *math-page-pad* 5
  "Logical pixels inside a pill or a button, on every edge.")

(defparameter *math-page-gap* 10
  "Logical pixels between the marks on a heading — the pill and the buttons.")

(defun %math-page-px (n)
  "N logical pixels as the device pixels a scene's lengths are in.

`*org-frozen-scale*' is *read* rather than copied. A scene's lengths are device
pixels and Lisp is not allowed to know the display's scale factor — measuring a
string in a real font is in the Rust column of the boundary — so that parameter
is the one number closing the gap, and a second copy of it here would be a second
number a config had to set and would eventually disagree with the first. 1 when
that file is not loaded, which is the only case in which nothing here draws
anyway."
  (max 0 (round (* n (if (boundp '*org-frozen-scale*)
                         (symbol-value '*org-frozen-scale*)
                         1)))))

(defun %math-page-mark (status)
  "(GLYPH FACE) for STATUS.

The fallback is not decoration. `math-set-status' writes whatever it is handed —
the refusal at the top of this file — so a curriculum spelling a third state must
draw as *something*, and a dot in the markup colour is the honest \"there is a
state here and it is not one of mine\". Without it a `paused' problem would come
out with no mark at all, which reads as `todo' and is a lie."
  (or (rest (assoc status *math-page-marks* :test #'string-equal))
      '("•" "markup")))

(defun %math-page-repeat (s n)
  "S written N times."
  (with-output-to-string (out) (dotimes (i (max 0 n)) (write-string s out))))

;;; ---------------------------------------------------------------------------
;;; One reading per page, not one per heading

(defvar *math-page* nil
  "The single reading of the buffer the page under construction is drawn from:

    :problems  every problem plist, in document order
    :units     every unit, each also carrying the `:done' and `:total' of the
               problems inside it
    :tags      problem `:begin' -> the scene tag its card was given

NIL when the frozen buffer is not a curriculum, which is most frozen buffers.

It exists because the hook is asked per *heading* and the answer is per
*document*. `%org-frozen-decorate' runs every function on
`*org-frozen-node-functions*' once for each heading it builds, so deriving a
problem from scratch there would be one `buffer-string' and one full outline scan
per heading — quadratic in the document, on a rebuild that happens every time a
status changes. One scan per page instead.")

(defvar *math-page-line* nil
  "The `:line' of the last heading decorated, which is how a *new* page is
recognised.

The hook has no \"the page starts here\" call and the headings of one page arrive
in document order, so the first heading whose line is not greater than the last
one seen is the first heading of a rebuild — and that is the moment to read the
buffer again. It cannot miss one: within a document the first heading's line is
never greater than the last's, so `<=' fires on every rebuild including a page
with exactly one heading and including a switch to a different document.

ponytail: a page boundary inferred rather than told. Ceiling: another function on
the same hook that walked headings backwards would defeat it, and nothing does —
`%org-frozen-nodes' is a forward loop over the groups. The upgrade path is a
`:kind :page' call at the top of `org-frozen-refresh', which is one line there
and one arm here.")

(defun %math-page-unit (unit problems)
  "UNIT with the `:done' and `:total' of the problems inside it.

LIST* onto the unit's own plist rather than a new one, so `:id', `:title',
`:begin' and `:end' are still exactly where `math-units' put them and this is a
unit that also knows its score."
  (let* ((inside (remove-if-not
                  (lambda (p) (and (>= (getf p :begin) (getf unit :begin))
                                   (<= (getf p :end) (getf unit :end))))
                  problems))
         (done (count *math-done* inside
                      :key (lambda (p) (getf p :status)) :test #'string=)))
    (list* :done done :total (length inside) unit)))

(defun %math-page-read ()
  "Everything the page needs about the live buffer, in one scan — or NIL when it
is not a curriculum.

One `%org-lines' feeds the curriculum test, the outline, the problems and the
units. That is the same discipline every reader above keeps, and it matters more
here: this runs inside a page build, so the cost is paid on entering the mode and
again on every write a program makes into the document."
  (let ((lines (%org-lines)))
    (when (math-curriculum-p lines)
      (let* ((headings (%math-headings lines))
             (problems (math-problems nil lines)))
        (list :problems problems
              :units (mapcar (lambda (u) (%math-page-unit u problems))
                             (math-units headings))
              :tags (make-hash-table))))))

;;; ---------------------------------------------------------------------------
;;; The seam

(defun math-problem-tag (problem)
  "The scene `:tag' a click on PROBLEM's card is handed back by — an integer, or
NIL when PROBLEM is NIL.

**This is the seam.** Anything drawing a curriculum as a scene asks here for a
problem's tag and hangs it on whatever nodes that problem occupies; the editor
hands the integer back on a click and `%scene-click' routes it to one closure,
which flips `:ZEMACS_STATUS:' between `todo' and `done'. Nothing on the drawing
side has to know what a status is or how one is written.

PROBLEM is a plist from `math-problems'. It is a *reading* and the closure does
not trust it: `math-set-status' re-finds the heading before it writes and says so
when it has moved, which is what makes a tag surviving one edit into the next
harmless rather than a write to the wrong place.

**One integer per problem per page.** The answer is remembered in the reading the
page was built from (`*math-page*'), so a pill and the word inside it can each be
tagged without allocating twice — which is the reason `scene-tag' is callable by
hand. Asked outside a page build there is no table and every call allocates,
which is correct rather than merely tolerated: a tag is only ever meaningful
against the page that was on screen when it was made.

A page rebuilt after a click gets fresh integers, because `scene-tag' retires the
last page's table the first time the next page asks for one. That is the same
reason a prompt id and a JSON-RPC request id are never reused: a stale integer
must name *nothing* rather than name something new."
  (when problem
    (let ((table (getf *math-page* :tags))
          (key (getf problem :begin)))
      (or (and table (gethash key table))
          (let ((tag (scene-tag (lambda () (%math-page-toggle problem)))))
            (when table (setf (gethash key table) tag))
            tag)))))

(defun %math-page-toggle (problem)
  "Flip PROBLEM between `todo' and `done'. What a click on its pill means.

`math-toggle-status' is the keyboard's version and starts from `(point)', which
is exactly what a scene does not have. This is the same decision reached from the
problem itself.

The status is read off the plist rather than re-scanned, and that is safe because
the page is rebuilt after every write: the plist a live pill was drawn from
describes the document as it is. Two clicks racing one rebuild both compute the
same target and the second write is the first written twice, which
`math-set-status' makes a no-op edit rather than a wrong one."
  (let ((want (if (string= (getf problem :status) *math-done*)
                  *math-todo* *math-done*)))
    (when (math-set-status problem want)
      (message (format nil "~a · ~a" (getf problem :kind) want)))))

(defun %math-page-invoke (name problem)
  "Call NAME with PROBLEM, or say which file was not loaded.

`math-code.lisp' loads *after* this one — it is written on top of the schema here
— so the two commands a programming problem's buttons reach cannot be named at
compile time. FBOUNDP rather than a bare call, because a config that loaded
`math.lisp' and not `math-code.lisp' is a legal config, and a button that
signalled into `%scene-click''s handler would report `scene click error' where
the true sentence is one line long."
  (if (fboundp name)
      (funcall name problem)
      (message (format nil "math: ~(~a~) is in runtime/modes/math-code.lisp, ~
                            which is not loaded"
                       name))))

(defun %math-page-answer (problem)
  "Leave the page with point on PROBLEM.

The one callback that hands control back to the grid, and the honest answer to
\"how do I write this one\": `goto-char' first, so the cursor is already on the
heading when the text comes back, and then `org-frozen-toggle', which is what
`SPC m z' runs. Both travel the same queue the editor drains in order, so the
move cannot land after the mode change.

The other route into a written answer needs no cursor at all — a photograph, and
`math-response-insert' writing the transcription — so this is the route for
somebody with a keyboard and a proof in their head."
  (goto-char (getf problem :begin))
  (org-frozen-toggle)
  (message "the manuscript is back — SPC m z returns to the page"))

;;; ---------------------------------------------------------------------------
;;; What a heading grows

(defun %math-page-button (label thunk)
  "LABEL as a box you can click, running THUNK.

The tag goes on the box *and* on the word inside it, which is what `scene-tag'
exists to be called by hand for. Hit testing may resolve to a block or to a run —
`docs/gui.org' hangs a `Tag' on both and the two are separate answers — and a
button hot on its glyphs and cold on its padding is the single thing that makes a
widget feel broken."
  (let ((tag (scene-tag thunk)))
    (block :background *math-page-tint* :pad (%math-page-px *math-page-pad*)
           :tag tag
           (text (run label :face "link" :tag tag)))))

(defun %math-page-action (problem)
  "The button saying what to do with PROBLEM next.

A *programming* problem has two, and they are the two keys it already has:
`SPC m x' runs it and `SPC m e' opens it beside the question. Both go through the
problem-taking siblings in `math-code.lisp' rather than the commands themselves,
for the reason at the head of this section — the point-reading versions would run
whichever problem the cursor was parked in.

A *written* problem has one and it leaves the page, because there is nowhere on a
page to write."
  (cond
    ((string-equal (getf problem :kind) *math-programming-kind*)
     (block :dir :row :gap (%math-page-px *math-page-gap*)
            (%math-page-button
             "▶ run" (lambda () (%math-page-invoke 'math-code-run-problem problem)))
            (%math-page-button
             "✎ open" (lambda () (%math-page-invoke 'math-code-edit-problem problem)))))
    (t (%math-page-button "✎ answer" (lambda () (%math-page-answer problem))))))

(defun %math-page-problem (problem)
  "The extra `block' arguments PROBLEM's heading grows: its mark, and the way on.

`:dir :row' turns the heading's own block on its side. The list answered here is
spliced into that block's argument list — keywords may interleave with children,
which is what `%scene-form' and the wire grammar both promise — so the heading
text and everything below become children of one row rather than a stack. The
`fill' spacer between them is what puts the marks out on the right margin, which
is where a printed page puts anything that is about the page rather than in it.

The pill's tag is `math-problem-tag''s, so a second renderer tagging the same
problem gets the same integer and one closure answers for both."
  (when problem
    (destructuring-bind (glyph face) (%math-page-mark (getf problem :status))
      (let ((tag (math-problem-tag problem)))
        (list :dir :row :align :center :gap (%math-page-px *math-page-gap*)
              (rect :width 'fill)
              (block :background *math-page-tint*
                     :pad (%math-page-px *math-page-pad*)
                     :tag tag
                     (text (run (format nil "~a ~a" glyph (getf problem :status))
                                :face face :tag tag)))
              (%math-page-action problem))))))

(defun %math-page-unit-nodes (unit)
  "The extra `block' arguments UNIT's heading grows: how much of it is done.

`math-progress' says this about the whole curriculum, in the status line, when
asked. This says it about *this* unit, on the heading being read, without being
asked — which is the entire difference between a number you can get and a number
you have.

A pip per problem and then the fraction, because the two answer different
questions: \"am I nearly through\" is a shape and \"how many are left\" is a
number. The pips are the same two glyphs the problems below them carry, so the
heading is a summary of the page under it rather than a second notation.

A unit with no problems grows nothing. `* Contents' is not a unit at all — it
carries no `:ID:' — but a unit of pure instruction is a real thing to write, and
`0/0' on it would be a score for a thing that is not scored.

ponytail: forty problems draw forty pips and push the count off the measure.
Ceiling: a curriculum shaped unlike the one this was built for — the units in
`docs/curriculum.org' carry two to five. The upgrade path is a `rect' of
`(pct (/ done total))' inside a track of a stated width, which is two nodes
instead of N and is what a progress bar is."
  (when (and unit (plusp (getf unit :total)))
    (let* ((done (getf unit :done))
           (total (getf unit :total))
           (yes (%math-page-mark *math-done*))
           (no (%math-page-mark *math-todo*)))
      (list :dir :row :align :center :gap (%math-page-px *math-page-gap*)
            (rect :width 'fill)
            (text (when (plusp done)
                    (run (%math-page-repeat (first yes) done) :face (second yes)))
                  (when (< done total)
                    (run (%math-page-repeat (first no) (- total done))
                         :face (second no)))
                  (run (format nil " ~d/~d" done total) :face "comment"))))))

(defun math-page-decorate (context)
  "Decorate one heading of a frozen page with what a curriculum means by it.

On `*org-frozen-node-functions*'. CONTEXT is that hook's plist and the answer is
a list of extra arguments spliced into the heading's `block' — the contract is
the docstring on the variable, and it is the reason `org-frozen.lisp' contains
not one word about problems while rendering them.

A heading is at most one of the two things this file knows: the `** Problem'
whose card it is, or the `* Unit' whose score it carries. Matched on `:begin' —
which the hook documents as the character offset of the heading line, and which
`math-problems' answers as the offset of its first star. The same number, so the
join is `eql' on an integer and involves no guess about the heading's text; a
curriculum that spells its problems `Exercise' works unchanged.

Returns NIL for every heading that is neither, which the hook reads as no
opinion — so a frozen buffer that is not a curriculum pays one scan per page and
draws exactly what it drew before."
  (when (eq (getf context :kind) :heading)
    (let ((line (getf context :line))
          (at (getf context :begin)))
      (when (or (null *math-page-line*) (<= line *math-page-line*))
        (setf *math-page* (%math-page-read)))
      (setf *math-page-line* line)
      (or (%math-page-problem
           (find at (getf *math-page* :problems) :key (lambda (p) (getf p :begin))))
          (%math-page-unit-nodes
           (find at (getf *math-page* :units) :key (lambda (u) (getf u :begin))))))))

;;; DEFVAR before the PUSHNEW, exactly as the two hook lists above: a build whose
;;; `*runtime-dir*' never found `org-frozen.lisp' must get an empty list here
;;; rather than an unbound variable, and a config reload must not double it.
(defvar *org-frozen-node-functions* nil)
(pushnew 'math-page-decorate *org-frozen-node-functions*)
