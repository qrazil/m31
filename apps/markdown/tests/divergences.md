Raw HTML is not in the subset, so a block of it is an escaped paragraph:

<div class="note">
  <p>hello</p>
</div>

<notascheme>

<not a link>

Inline raw HTML is escaped too: a <span>span</span> and a <br> break.

Entity references are not decoded: &amp; &copy; &#35; &#x2764;

Link reference definitions are not supported, so this is a paragraph:

[ref]: /url "title"

and [this][ref] is not a link, nor is [this] on its own.
