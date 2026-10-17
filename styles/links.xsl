<?xml version="1.0"?>
<xsl:stylesheet version="1.0" xmlns:xsl="http://www.w3.org/1999/XSL/Transform">
  <!-- The room's shared templates. Not a view: `urn:cms:style:{name}` splices every template here
       into each view stylesheet before serving it (native.rs `style_source`), because the XSLT
       engine resolves no xsl:include. So there is ONE copy of each rule, and every view calls it
       with xsl:call-template.

       `safe-link` is the room's one rule for a link whose target comes from library data (a
       bookmark's URL, a book's reader URL, a dead link under review), ledger #1793. It links
       only when the target, past any leading whitespace (which a browser strips too), starts
       with `http://` or `https://` (in any case), or with a single `/` (a path on the room's
       own origin; `//` and `/\` name another host). Anything else,
       `javascript:`, `data:`, `vbscript:`, `file:` or no scheme at all, renders the label as text
       in a span with the same class, so the card still reads. `safe_href` in native.rs is the
       same rule for markup built in Rust, and a test holds the two to the same answers.

       Authored to xrust's subset: no xsl:variable, no string-length, and no character references
       in an XPath literal (xrust reads `&#9;` there as a space), so the rule cannot also refuse
       a tab or newline inside the target. A browser strips those, so `/` + tab + `/host` opens
       another host; it is still an http(s) navigation, the same as any https link a card may
       carry, and never a script. -->
  <xsl:template name="safe-link">
    <xsl:param name="href"/>
    <xsl:param name="class"/>
    <xsl:param name="label"/>
    <xsl:choose>
      <xsl:when test="starts-with(translate(normalize-space($href), 'HTPS', 'htps'), 'https://') or starts-with(translate(normalize-space($href), 'HTPS', 'htps'), 'http://') or (starts-with(normalize-space($href), '/') and not(starts-with(normalize-space($href), '//')) and not(starts-with(normalize-space($href), '/\')))">
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
