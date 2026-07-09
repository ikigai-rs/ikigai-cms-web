<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <xsl:template match="/rdf:RDF">
    <style>.cms-count{color:var(--mut);font-family:ui-monospace,monospace;font-size:12px;margin-bottom:6px}
.cms-card{display:flex;align-items:baseline;gap:10px;font-family:ui-monospace,monospace;padding:7px 2px;border-bottom:1px solid var(--line)}
.cms-title{flex:1;color:var(--fg);text-decoration:none;font-size:13px;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.cms-title:hover{text-decoration:underline}
.cms-tags{display:flex;gap:6px}
.cms-tag{font-size:12px;color:var(--mut);text-decoration:none;cursor:pointer}</style>
    <section class="cms-room">
      <div class="cms-count"><xsl:value-of select="count(rdf:Description)"/> resources</div>
      <xsl:apply-templates select="rdf:Description"/>
    </section>
  </xsl:template>

  <xsl:template match="rdf:Description">
    <article class="cms-card">
      <a class="cms-title">
        <xsl:attribute name="href"><xsl:value-of select="dc:identifier"/></xsl:attribute>
        <xsl:value-of select="dc:title"/>
      </a>
      <div class="cms-tags">
        <xsl:for-each select="dc:subject">
          <a class="cms-tag" hx-target="#room">
            <xsl:attribute name="hx-get">urn:cms:view:<xsl:value-of select="."/></xsl:attribute>
            <xsl:text>#</xsl:text><xsl:value-of select="."/>
          </a>
        </xsl:for-each>
      </div>
    </article>
  </xsl:template>
</xsl:stylesheet>
