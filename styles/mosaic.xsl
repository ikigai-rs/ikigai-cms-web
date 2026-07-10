<?xml version="1.0"?>
<xsl:stylesheet version="1.0"
    xmlns:xsl="http://www.w3.org/1999/XSL/Transform"
    xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
    xmlns:dc="http://purl.org/dc/elements/1.1/">
  <xsl:output method="html" omit-xml-declaration="yes"/>

  <xsl:template match="/rdf:RDF">
    <style>.cms-room{display:grid;grid-template-columns:repeat(auto-fit,minmax(210px,1fr));gap:12px}
.cms-count{grid-column:1/-1;color:var(--mut);font-size:13px}
.cms-card{background:var(--card);border:1px solid var(--line);border-radius:12px;padding:12px 14px}
.cms-title{display:block;color:var(--fg);text-decoration:none;font-size:14px;font-weight:500;line-height:1.4;margin-bottom:8px}
.cms-tags{display:flex;flex-wrap:wrap;gap:5px}
.cms-tag{font-size:11px;padding:2px 9px;border-radius:20px;background:var(--accent-bg);color:var(--accent);text-decoration:none;cursor:pointer}</style>
    <section class="cms-room">
      <div class="cms-count"><xsl:value-of select="count(rdf:Description)"/> resources</div>
      <xsl:apply-templates select="rdf:Description"/>
    </section>
  </xsl:template>

  <xsl:template match="rdf:Description">
    <article class="cms-card">
      <a class="cms-title" target="_blank" rel="noopener noreferrer">
        <xsl:attribute name="href"><xsl:value-of select="dc:identifier"/></xsl:attribute>
        <xsl:value-of select="dc:title"/>
      </a>
      <div class="cms-tags">
        <xsl:for-each select="dc:subject">
          <a class="cms-tag" hx-target="#room">
            <xsl:attribute name="hx-get">urn:cms:view:<xsl:value-of select="."/></xsl:attribute>
            <xsl:value-of select="."/>
          </a>
        </xsl:for-each>
      </div>
    </article>
  </xsl:template>
</xsl:stylesheet>
