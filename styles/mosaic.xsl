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
.cms-sug{display:inline-flex;align-items:center;gap:4px;font-size:11px;color:var(--mut);border:1px dashed var(--line);border-radius:20px;padding:1px 4px 1px 8px}
.cms-sug-yes,.cms-sug-no{font:11px ui-monospace,monospace;border:none;background:transparent;cursor:pointer;padding:0 3px;line-height:1}
.cms-sug-yes{color:#3a8a3a}.cms-sug-no{color:#c0392b}</style>
    <section class="cms-room">
      <div class="cms-count"><xsl:value-of select="count(rdf:Description)"/> resources</div>
      <xsl:apply-templates select="rdf:Description"/>
    </section>
  </xsl:template>

  <xsl:template match="rdf:Description">
    <article class="cms-card">
      <xsl:attribute name="data-kind"><xsl:value-of select="cms:kind"/></xsl:attribute>
      <span class="cms-kind"><xsl:value-of select="cms:kind"/></span>
      <a class="cms-title" target="_blank" rel="noopener noreferrer">
        <xsl:attribute name="href"><xsl:value-of select="dc:identifier"/></xsl:attribute>
        <xsl:value-of select="dc:title"/>
      </a>
      <div class="cms-byline"><xsl:apply-templates select="dc:creator"/></div>
      <div class="cms-tags">
        <xsl:for-each select="dc:subject">
          <a class="cms-tag" hx-target="#room">
            <xsl:attribute name="hx-get">/r/urn:cms:view:<xsl:value-of select="."/></xsl:attribute>
            <xsl:value-of select="."/>
          </a>
        </xsl:for-each>
        <xsl:for-each select="cms:suggestedTag">
          <span class="cms-sug">
            <xsl:text>#</xsl:text><xsl:value-of select="."/>
            <button class="cms-sug-yes" hx-post="/tag/approve" hx-target="closest .cms-sug" hx-swap="outerHTML" title="promote to a tag">
              <xsl:attribute name="hx-vals">{"book":"<xsl:value-of select="../@rdf:about"/>","tag":"<xsl:value-of select="."/>"}</xsl:attribute>
              <xsl:text>+</xsl:text>
            </button>
            <button class="cms-sug-no" hx-post="/tag/reject" hx-target="closest .cms-sug" hx-swap="outerHTML" title="dismiss">
              <xsl:attribute name="hx-vals">{"book":"<xsl:value-of select="../@rdf:about"/>","tag":"<xsl:value-of select="."/>"}</xsl:attribute>
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
