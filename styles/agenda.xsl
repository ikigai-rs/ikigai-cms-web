<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
    xmlns:cms="https://ikigai-rs.dev/ns/cms#"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <xsl:template match="/rdf:RDF">
    <style>.cms-count{color:var(--mut);font-family:ui-monospace,monospace;font-size:12px;margin-bottom:6px}
.cms-card{display:flex;align-items:baseline;gap:10px;font-family:ui-monospace,monospace;padding:7px 2px;border-bottom:1px solid var(--line)}
.cms-title{flex:1;color:var(--fg);text-decoration:none;font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.cms-title:hover{text-decoration:underline}
.cms-author{color:var(--mut);font-size:12px;font-style:italic;white-space:nowrap}
.cms-tags{display:flex;gap:6px}
.cms-tag{font-size:12px;color:var(--mut);text-decoration:none;cursor:pointer}
.cms-sug{display:inline-flex;align-items:center;gap:4px;font-size:12px;color:var(--mut);border:1px dashed var(--line);border-radius:10px;padding:0 4px 0 7px}
.cms-sug-yes,.cms-sug-no{font:12px ui-monospace,monospace;border:none;background:transparent;cursor:pointer;padding:0 3px;line-height:1}
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
      <xsl:apply-templates select="dc:creator"/>
      <div class="cms-tags">
        <xsl:for-each select="dc:subject">
          <a class="cms-tag" hx-target="#room">
            <xsl:attribute name="hx-get">/r/urn:cms:view:<xsl:value-of select="."/></xsl:attribute>
            <xsl:text>#</xsl:text><xsl:value-of select="."/>
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
