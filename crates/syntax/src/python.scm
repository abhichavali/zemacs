; Python highlights. tree-sitter-python ships a `highlights.scm'; this replaces
; it, because that one is eighteen capture names thin and has nothing at all to
; say about decorators, docstrings, parameters, `self' or brackets — so a Python
; buffer arrived with a handful of colours in it where Emacs' `python-ts-mode',
; which ignores the bundled query in favour of its own twenty-feature ruleset,
; has a dozen. This is that ruleset, spelled in the capture names `CAPTURES' in
; lib.rs already folds onto faces.
;
; Two rules decide who wins, and they are not the same rule:
;
; * when several patterns capture the same node the LAST one wins, so general
;   patterns come first and specific ones override them further down;
; * a capture nested *inside* another covers it for as long as it lasts, and
;   that beats order — which is why the decorator patterns at the bottom name
;   their inner identifiers rather than trusting `(decorator) @type' to paint
;   through the `@property' capture underneath it.
;
; The one thing removed rather than added is the stock query's `(identifier)
; @variable', which painted every name in the file. In the themes that give
; `variable' a colour of its own that was a wall of cyan, and it is the opposite
; of what a theme is for: mark the names that are something in particular — a
; parameter, a property, a builtin — and leave the rest as body text.
; python-ts-mode agrees, plain variable *uses* being its level 4, one past the
; level anybody runs.

(comment) @comment

(string) @string
(escape_sequence) @escape

[(integer) (float)] @number
[(none) (true) (false)] @constant.builtin

; An f-string is a string, but what is between the braces is code: `@variable'
; is the body colour, and the calls, numbers and attributes in there paint over
; it from the inside.
(interpolation) @variable
(interpolation "{" @punctuation.special "}" @punctuation.special)

[
  "-" "-=" "!=" "*" "**" "**=" "*=" "/" "//" "//=" "/=" "&" "&=" "%" "%="
  "^" "^=" "+" "+=" "->" "<" "<<" "<<=" "<=" "<>" "=" ":=" "==" ">" ">="
  ">>" ">>=" "|" "|=" "~" "@="
] @operator

; `and', `in', `is', `not' and `or' are operators in the grammar and keywords
; here, which is a deliberate copy of python-ts-mode: its own comment on the
; list says "these are technically operators, but we fontify them as keywords",
; and `if x not in xs' reads the way it is spoken.
[
  "and" "as" "assert" "async" "await" "break" "case" "class" "continue" "def"
  "del" "elif" "else" "except" "exec" "finally" "for" "from" "global" "if"
  "import" "in" "is" "is not" "lambda" "match" "nonlocal" "not" "not in" "or"
  "pass" "print" "raise" "return" "try" "type" "while" "with" "yield"
] @keyword

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," "." ":" ";" (ellipsis)] @punctuation.delimiter

; --- names -----------------------------------------------------------------

(attribute attribute: (identifier) @property)

; Parameters, in each shape the grammar wraps one in: plain, annotated,
; defaulted, both at once, `*args' and `**kwargs'. The two splat patterns are
; also assignment targets (`*rest, last = xs'), which wants the same face for
; the same reason — a name being bound rather than read.
(parameters (identifier) @variable)
(lambda_parameters (identifier) @variable)
(typed_parameter (identifier) @variable)
(default_parameter name: (identifier) @variable)
(typed_default_parameter name: (identifier) @variable)
(list_splat_pattern (identifier) @variable)
(dictionary_splat_pattern (identifier) @variable)

(assignment left: (identifier) @variable)
(augmented_assignment left: (identifier) @variable)
(for_statement left: (identifier) @variable)
(pattern_list (identifier) @variable)
(tuple_pattern (identifier) @variable)

; `self' and `cls' are the method protocol rather than names anybody picked, and
; they arrive as parameters — so this has to outrank the rule above. Keyword,
; which is where python-ts-mode puts them too.
((identifier) @variable.builtin
 (#match? @variable.builtin "^(self|cls)$"))

; `__name__', `__all__', `__dict__' — a name the language owns rather than one
; in this file. Above the call and definition rules, so `def __init__' is still
; read as the definition it is.
((identifier) @constant.builtin
 (#match? @constant.builtin "^__[a-zA-Z0-9_]+__$"))

; The whole annotation, not just the bare name the stock query stopped at:
; `list[int]' and `Foo | None' are types all the way through.
(type) @type

; --- calls and definitions --------------------------------------------------

(call function: (identifier) @function)
(call function: (attribute attribute: (identifier) @function.method))

; `len' is not one of your functions, and `CAPTURES' gives `function.builtin' a
; face of its own to say so. Call position only, which is what keeps a local
; named `id' or `type' from going the same colour.
((call function: (identifier) @function.builtin)
 (#match?
   @function.builtin
   "^(abs|all|any|ascii|bin|bool|breakpoint|bytearray|bytes|callable|chr|classmethod|compile|complex|delattr|dict|dir|divmod|enumerate|eval|exec|filter|float|format|frozenset|getattr|globals|hasattr|hash|help|hex|id|input|int|isinstance|issubclass|iter|len|list|locals|map|max|memoryview|min|next|object|oct|open|ord|pow|print|property|range|repr|reversed|round|set|setattr|slice|sorted|staticmethod|str|sum|super|tuple|type|vars|zip|__import__)$"))

; CamelCase is a class and ALL_CAPS is a constant. Heuristics, but the two every
; Python reader already runs — and they come after the call rules on purpose, so
; `Foo()' reads as the constructor rather than as one more function.
((identifier) @type
 (#match? @type "^[A-Z]"))
((identifier) @constant
 (#match? @constant "^[A-Z][A-Z0-9_]*$"))

(function_definition name: (identifier) @function)
(class_definition name: (identifier) @type)

; A docstring is prose and not data. Emacs gives it `font-lock-doc-face'; here
; `string.documentation' folds onto the comment face, which is the same idea
; with one face fewer. The first statement of a module, class or function body —
; the anchor is what "first" means.
(module . (expression_statement (string) @string.documentation))
(class_definition
  body: (block . (expression_statement (string) @string.documentation)))
(function_definition
  body: (block . (expression_statement (string) @string.documentation)))

; A decorator is a mark on the definition below it rather than a step in the
; flow, and Emacs paints all of it — `@', dotted path and arguments — with
; `font-lock-type-face'. The arguments keep their own colours, because they are
; nested inside and nesting wins.
;
; ponytail: the inner names are spelled out one dot deep, so `@app.route' and
; `@pytest.fixture(scope="module")' come out whole and `@a.b.c' leaves its `b'
; in the property colour. The fix is a pattern per dot; there is no third dot in
; real code.
(decorator) @type
(decorator (identifier) @type)
(decorator (attribute (identifier) @type))
(decorator (call function: (identifier) @type))
(decorator (call function: (attribute (identifier) @type)))
