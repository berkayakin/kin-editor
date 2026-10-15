; (), [] and {} pair by the grammar's tokens without a query.
(string "'" @open "'" @close)
(encapsed_string "\"" @open "\"" @close)

; The alternative syntax: `if (…): … endif;`
(if_statement "if" @open "endif" @close)
(foreach_statement "foreach" @open "endforeach" @close)
(for_statement "for" @open "endfor" @close)
(while_statement "while" @open "endwhile" @close)
(switch_statement "switch" @open body: (switch_block "endswitch" @close))
