<?xml version="1.0"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <!-- The room's shared templates. Not a view: `urn:cms:style:{name}` splices every template here
       into each view stylesheet before serving it (native.rs `style_source`), because the XSLT
       engine resolves no xsl:include. So there is ONE copy of each rule, and every view calls it
       with xsl:call-template.

       `safe-link` is the room's one rule for a link whose target comes from library data (a
       bookmark's URL, a book's reader URL, a dead link under review), FND-H's `safe_href`
       (ikigai_fn::html, ledger #1813) as XSLT. It links only when the target, past any leading
       and trailing whitespace (which a browser strips too), starts with `http://` or `https://`
       (in any case), or with a single `/` (a path on the room's own origin; `//` and `/\` name
       another host), AND holds no tab, line feed or carriage return between its first and last
       visible character and no DEL anywhere: a browser deletes a tab or newline inside a URL
       before reading it, so `/` + tab + `/host` opens another host and `java` + tab + `script:`
       runs. Anything else, `javascript:`, `data:`, `vbscript:`, `file:` or no scheme at all,
       renders the label as text in a span with the same class, so the card still reads. A test
       (native.rs, `the_stylesheet_link_rule_gives_the_shared_vectors_answers`) runs every
       `safe_href` case of ikigai-fn's shared vectors (tests/vectors/html_vectors.json) through it.

       Authored to xrust's subset: no xsl:variable and no string-length, and no whitespace
       character in an XPath literal, since xrust reads `&#9;` (or `&#160;`, or a literal U+00A0)
       there as a space. So the inner-whitespace test needs none: delete the spaces, normalize,
       and any space left came from a tab, line feed or carriage return between two visible
       characters. `&#127;` (DEL) is not whitespace and reads correctly. Known differences from
       the shared rule, both from xrust's `normalize-space` treating U+00A0 as whitespace: a
       target with a LEADING no-break space links here (the shared rule refuses it; a browser
       reads it as a relative path on the room's origin, so it stays harmless), and one with a
       no-break space INSIDE does not (the shared rule links it). Element text carries every
       other vector exactly; an attribute does not, as xrust reads a tab or newline character
       reference in an attribute value as a space. -->
  <xsl:template name="safe-link">
    <xsl:param name="href"/>
    <xsl:param name="class"/>
    <xsl:param name="label"/>
    <xsl:choose>
      <xsl:when test="(starts-with(translate(normalize-space($href), 'HTPS', 'htps'), 'https://') or starts-with(translate(normalize-space($href), 'HTPS', 'htps'), 'http://') or (starts-with(normalize-space($href), '/') and not(starts-with(normalize-space($href), '//')) and not(starts-with(normalize-space($href), '/\')))) and not(contains(normalize-space(translate($href, ' ', '')), ' ')) and not(contains($href, '&#127;'))">
        <a target="_blank" rel="noopener noreferrer">
          <xsl:attribute name="class"><xsl:value-of select="$class"/></xsl:attribute>
          <xsl:attribute name="href"><xsl:value-of select="$href"/></xsl:attribute>
          <xsl:value-of select="$label"/>
        </a>
      </xsl:when>
      <xsl:otherwise>
        <span>
          <xsl:attribute name="class"><xsl:value-of select="$class"/></xsl:attribute>
          <xsl:value-of select="$label"/>
        </span>
      </xsl:otherwise>
    </xsl:choose>
  </xsl:template>
</xsl:stylesheet>
