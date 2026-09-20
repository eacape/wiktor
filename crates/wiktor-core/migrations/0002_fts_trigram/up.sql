-- 迁移 0002：pages_fts 重建为 trigram 分词。
-- 背景：unicode61 对无空格 CJK 文本几乎无效（整段中文 = 1 个 token），
-- 奶茶检索全是中文短语，必须换 trigram（SQLite 3.34+，按 3-codepoint 切分）。
-- DROP pages_fts 会级联删除依赖它的 3 个触发器；但 0001 已在库中建同名触发器，
-- 必须显式 DROP 后再重建，避免 "trigger already exists"。
DROP TABLE IF EXISTS pages_fts;
DROP TRIGGER IF EXISTS pages_fts_insert;
DROP TRIGGER IF EXISTS pages_fts_update;
DROP TRIGGER IF EXISTS pages_fts_delete;

CREATE VIRTUAL TABLE pages_fts USING fts5(
    page_id UNINDEXED,
    entity_id UNINDEXED,
    title,
    content,
    tokenize = 'trigram'
);

CREATE TRIGGER pages_fts_insert AFTER INSERT ON pages BEGIN
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_update AFTER UPDATE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
    INSERT INTO pages_fts (page_id, entity_id, title, content)
    VALUES (NEW.page_id, NEW.entity_id, NEW.title, NEW.content);
END;
CREATE TRIGGER pages_fts_delete AFTER DELETE ON pages BEGIN
    DELETE FROM pages_fts WHERE page_id = OLD.page_id;
END;

-- 回填存量页面（fresh 库空操作，语义安全）
INSERT INTO pages_fts (page_id, entity_id, title, content)
    SELECT page_id, entity_id, title, content FROM pages;
