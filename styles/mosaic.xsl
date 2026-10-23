<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
    xmlns:cms="https://ikigai-rs.dev/ns/cms#"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <xsl:template match="/rdf:RDF">
    <style>.cms-room{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:12px}
.cms-count{grid-column:1/-1;color:var(--mut);font-size:13px}
.cms-card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px 14px}
.cms-title{display:block;color:var(--fg);text-decoration:none;font-size:14px;font-weight:500;line-height:1.4;margin-bottom:4px}
.cms-author{display:inline-block;color:var(--mut);font-size:12px;font-style:italic;margin:0 6px 6px 0}
.cms-tags{display:flex;flex-wrap:wrap;gap:5px}
.cms-tag{font-size:11px;padding:2px 9px;border-radius:20px;background:var(--accent-bg);color:var(--accent);text-decoration:none;cursor:pointer}
.cms-sug{display:inline-flex;align-items:center;gap:4px;font-size:11px;color:var(--mut);border:1px dashed var(--line);border-radius:20px;padding:0 2px 0 8px}
.cms-sug-yes,.cms-sug-no{font:11px ui-monospace,monospace;border:none;background:transparent;cursor:pointer;padding:0;line-height:1;min-width:24px;min-height:24px;display:inline-flex;align-items:center;justify-content:center}
.cms-sug-yes{color:#3a8a3a}.cms-sug-no{color:#c0392b}</style>
    <section class="cms-room">
      <div class="cms-count"><xsl:value-of select="count(rdf:Description)"/> resources</div>
      <!-- The cards in the order the reader asked for. The page query chose them, but a CONSTRUCT
           answers a graph, which has no order, so each card carries its sort key and the
           direction (native.rs sort_construct). xrust takes no expression in order=, hence the choose. -->
      <xsl:choose>
        <xsl:when test="rdf:Description[position() = 1]/cms:sortDir = 'desc'">
          <xsl:apply-templates select="rdf:Description"><xsl:sort select="cms:sortKey" order="descending"/></xsl:apply-templates>
        </xsl:when>
        <xsl:otherwise>
          <xsl:apply-templates select="rdf:Description"><xsl:sort select="cms:sortKey"/></xsl:apply-templates>
        </xsl:otherwise>
      </xsl:choose>
    </section>
  </xsl:template>

  <xsl:template match="rdf:Description">
    <article class="cms-card">
      <xsl:attribute name="data-kind"><xsl:value-of select="cms:kind"/></xsl:attribute>
      <span class="cms-kind"><xsl:value-of select="cms:kind"/></span>
      <!-- The link target is library data: the shared rule decides whether it may be a link
           (styles/links.xsl, ledger #1793). -->
      <xsl:call-template name="safe-link">
        <xsl:with-param name="href" select="string(dc:identifier)"/>
        <xsl:with-param name="class" select="'cms-title'"/>
        <xsl:with-param name="label" select="string(dc:title)"/>
      </xsl:call-template>
      <div class="cms-byline"><xsl:apply-templates select="dc:creator"/></div>
      <div class="cms-tags">
        <!-- Each `cms:tagChip` is "{the tag, percent-encoded} {the tag}" (native.rs TAG_CHIP_WHERE):
             SPARQL's ENCODE_FOR_URI writes no space, so the first space splits the path segment
             from the label, and a `?`, `#` or `/` in a tag cannot end the path (ledger #1794). -->
        <xsl:for-each select="cms:tagChip">
          <a class="cms-tag" hx-target="#room">
            <!-- The href is the hx-get: htmx swaps in place on click and Enter, and the href puts the
                 chip in the tab order (an <a> with no href takes no focus; ledger #1640). -->
            <xsl:attribute name="href">/r/urn:cms:view:<xsl:value-of select="substring-before(., ' ')"/></xsl:attribute>
            <xsl:attribute name="hx-get">/r/urn:cms:view:<xsl:value-of select="substring-before(., ' ')"/></xsl:attribute>
            <xsl:value-of select="substring-after(., ' ')"/>
          </a>
        </xsl:for-each>
        <xsl:for-each select="cms:suggestedTag">
          <span class="cms-sug">
            <xsl:text>#</xsl:text><xsl:value-of select="."/>
            <!-- `book` and `tag` ride as hidden inputs the buttons hx-include, so htmx form-encodes
                 each value itself. They were an hx-vals JSON object assembled here, and attribute
                 escaping protects the HTML, not the JSON: a `"` in a tag added keys (ledger #1794). -->
            <input type="hidden" name="book"><xsl:attribute name="value"><xsl:value-of select="../@rdf:about"/></xsl:attribute></input>
            <input type="hidden" name="tag"><xsl:attribute name="value"><xsl:value-of select="."/></xsl:attribute></input>
            <button class="cms-sug-yes" hx-post="/tag/approve" hx-include="closest .cms-sug" hx-target="closest .cms-sug" hx-swap="outerHTML" title="promote to a tag">
              <xsl:text>+</xsl:text>
            </button>
            <button class="cms-sug-no" hx-post="/tag/reject" hx-include="closest .cms-sug" hx-target="closest .cms-sug" hx-swap="outerHTML" title="dismiss">
              <xsl:text>x</xsl:text>
            </button>
          </span>
        </xsl:for-each>
      </div>
    </article>
  </xsl:template>

  <!-- Authors (books carry dc:creator; bookmarks don't, so this renders nothing there). -->
  <xsl:template match="dc:creator">
    <span class="cms-author"><xsl:value-of select="."/></span>
  </xsl:template>
</xsl:stylesheet>
