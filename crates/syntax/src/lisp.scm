; Common Lisp highlights. tree-sitter-commonlisp ships a `tags.scm` but no
; `highlights.scm`, so this file is ours.
;
; Order matters: when several patterns capture the same node, tree-sitter-highlight
; keeps the LAST one, so general patterns come first and specific ones override
; them further down.

(comment) @comment
(block_comment) @comment

(str_lit) @string
(num_lit) @number
(complex_num_lit) @number

(char_lit) @constant
(nil_lit) @constant
(kwd_lit) @constant

(package_lit) @type

; (defun foo ...), (defmacro foo ...), (defvar *x* ...) — the grammar gives all
; of these a `defun` wrapper node with a keyword and a name field.
(defun_keyword) @keyword
(defun_header function_name: (sym_lit) @function)

; The symbol at the head of a form is being called. The anchor `.` counts named
; children only, so the opening paren doesn't get in the way.
(list_lit . (sym_lit) @function)

; ...unless the head is a special operator or a standard macro, which only read
; as keywords in head position — `list` is a function everywhere, but `loop` is
; a keyword only at the head of a form. Last pattern to touch the head symbol,
; so it wins over both rules above.
(list_lit
  .
  (sym_lit) @keyword
  (#match? @keyword "^(let\\*?|labels|flet|macrolet|symbol-macrolet|lambda|if|when|unless|cond|case|ccase|ecase|typecase|etypecase|progn|prog1|prog2|block|return|return-from|tagbody|go|catch|throw|unwind-protect|handler-case|handler-bind|restart-case|multiple-value-bind|destructuring-bind|with-open-file|with-slots|with-accessors|setf|setq|psetf|psetq|push|pushnew|pop|incf|decf|declare|declaim|proclaim|the|quote|function|and|or|not|loop|do|do\\*|dolist|dotimes|eval-when|in-package|defpackage|require|provide|assert|check-type|values|multiple-value-call|load-time-value|locally|otherwise)$"))

; `t` and `nil` are self-evaluating, wherever they turn up — including as the
; catch-all head of a `cond` clause, which is why this outranks the rule above.
((sym_lit) @constant
 (#match? @constant "^([tT]|[nN][iI][lL])$"))

; `(let ((x 1) (y 2)) ...)` — the binding list is not a pile of calls, so undo
; the head-of-form rule inside it; `let` is common enough that getting this
; wrong is the most visible mistake the heuristic can make. This has to be the
; last pattern touching the head symbol *and* re-capture it as @keyword: when a
; later pattern overrides a node, tree-sitter-highlight discards the whole
; earlier match, which would take the @variable captures down with it.
((list_lit
   .
   (sym_lit) @keyword
   .
   (list_lit (list_lit . (sym_lit) @variable)))
 (#match? @keyword "^(let\\*?|do\\*?|symbol-macrolet)$"))

["(" ")"] @punctuation.bracket
