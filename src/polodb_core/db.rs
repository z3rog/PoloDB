// root_btree schema
// {
//   _id: ObjectId,
//   name: String,
//   root_pid: Int,
//   flags: Int,
// }
//
// flags indicates:
// key_ty: 1byte
// ...
//
use std::rc::Rc;
use std::collections::LinkedList;
use super::error::DbErr;
use super::page::{ header_page_utils, PageHandler };
use crate::bson::ObjectIdMaker;
use crate::overflow_data::{ OverflowDataWrapper, OverflowDataTicket };
use crate::bson::{ObjectId, Document, Value};
use crate::btree::BTreePageWrapper;
use crate::cursor::Cursor;

// #[derive(Clone)]
pub struct Database {
    ctx: Box<DbContext>,
}

pub type DbResult<T> = Result<T, DbErr>;

pub(crate) struct DbContext {
    page_handler :        Box<PageHandler>,
    pending_block_offset: u32,
    overflow_data_pages:  LinkedList<u32>,

    pub obj_id_maker: ObjectIdMaker,

}

impl DbContext {

    fn new(path: &str) -> DbResult<DbContext> {
        let page_size = 4096;

        let page_handler = PageHandler::new(path, page_size)?;

        let obj_id_maker = ObjectIdMaker::new();

        let ctx = DbContext {
            page_handler: Box::new(page_handler),

            pending_block_offset: 0,
            overflow_data_pages: LinkedList::new(),

            // first_page,
            obj_id_maker,
        };
        Ok(ctx)
    }

    fn alloc_overflow_ticker(&mut self, size: u32) -> DbResult<OverflowDataTicket> {
        let page_id = self.page_handler.alloc_page_id()?;

        self.overflow_data_pages.push_back(page_id);

        let raw_page = self.page_handler.pipeline_read_page(page_id)?;

        let mut overflow = OverflowDataWrapper::from_raw_page(&mut self.page_handler, raw_page)?;

        let ticket = overflow.alloc(size)?;

        Ok(OverflowDataTicket {
            items: vec![ ticket ],
        })
    }

    #[inline]
    fn get_meta_page_id(&mut self) -> DbResult<u32> {
        let head_page = self.page_handler.pipeline_read_page(0)?;
        let result = header_page_utils::get_meta_page_id(&head_page);

        if result == 0 {  // unexpected
            return Err(DbErr::MetaPageIdError);
        }

        Ok(result)
    }

    pub fn create_collection(&mut self, name: &str) -> DbResult<ObjectId> {
        let oid = self.obj_id_maker.mk_object_id();
        let mut doc = Document::new_without_id();
        doc.insert("_id".into(), Value::ObjectId(oid.clone()));

        doc.insert("name".into(), Value::String(name.into()));

        let root_pid = self.page_handler.alloc_page_id()?;
        doc.insert("root_pid".into(), Value::Int(root_pid as i64));

        doc.insert("flags".into(), Value::Int(0));

        let meta_page_id: u32 = self.get_meta_page_id()?;

        let mut btree_wrapper = BTreePageWrapper::new(&mut self.page_handler, meta_page_id);

        let backward = btree_wrapper.insert_item(Rc::new(doc), false)?;

        match backward {
            Some(backward_item) => {
                let new_root_id = self.page_handler.alloc_page_id()?;

                let raw_page = backward_item.write_to_page(new_root_id, meta_page_id, self.page_handler.page_size)?;

                // update head page
                {
                    let mut head_page = self.page_handler.pipeline_read_page(0)?;
                    header_page_utils::set_meta_page_id(&mut head_page, new_root_id);
                    self.page_handler.pipeline_write_page(&head_page)?;
                }

                self.page_handler.pipeline_write_page(&raw_page)?;

                Ok(oid)
            }

            None => Ok(oid)
        }
    }

    fn insert(&mut self, col_name: &str, mut doc: Rc<Document>) -> DbResult<()> {
        let meta_page_id = self.get_meta_page_id()?;
        let mut cursor = Cursor::new(&mut self.page_handler, meta_page_id)?;

        let doc = {
            let id = doc.get("_id");
            match id {
                Some(val) => doc,
                None => {
                    let new_doc = Rc::make_mut(&mut doc);
                    new_doc.insert("_id".into(), Value::ObjectId(self.obj_id_maker.mk_object_id()));
                    doc
                }
            }
        };

        cursor.insert(col_name, doc)
    }

    fn get_collection_cursor(&mut self, col_name: &str) -> DbResult<Cursor> {
        let root_page_id: i64 = {
            let meta_page_id = self.get_meta_page_id()?;
            let mut cursor = Cursor::new(&mut self.page_handler, meta_page_id)?;

            let mut tmp: i64 = -1;

            while cursor.has_next() {
                let doc = cursor.peek().unwrap();

                let doc_name = match doc.get("name") {
                    Some(name) => name,
                    None => return Err(DbErr::CollectionNotFound(col_name.into()))
                };

                if let Value::String(str_content) = doc_name {
                    if str_content == col_name {
                        tmp = match doc.get("root_pid") {
                            Some(Value::Int(pid)) => *pid,
                            _ => -1,
                        };
                        break;
                    }
                }

                let _ = cursor.next()?;
            }

            if tmp < 0 {
                return Err(DbErr::CollectionNotFound(col_name.into()))
            }

            tmp
        };

        Ok(Cursor::new(&mut self.page_handler, root_page_id as u32)?)
    }

    pub fn query_all_meta(&mut self) -> DbResult<Vec<Rc<Document>>> {
        let meta_page_id = self.get_meta_page_id()?;

        let mut result = vec![];
        let mut cursor = Cursor::new(&mut self.page_handler, meta_page_id)?;

        while cursor.has_next() {
            result.push(cursor.peek().unwrap());

            let _ = cursor.next()?;
        }

        Ok(result)
    }

}

impl Drop for DbContext {

    fn drop(&mut self) {
        let _ = self.page_handler.checkpoint_journal();  // ignored
    }

}

impl Database {

    pub fn open(path: &str) -> DbResult<Database>  {
        let ctx = DbContext::new(path)?;
        let rc_ctx = Box::new(ctx);

        Ok(Database {
            ctx: rc_ctx,
        })
    }

    pub fn create_collection(&mut self, name: &str) -> DbResult<ObjectId> {
        self.ctx.create_collection(name)
    }

    pub fn get_version(&self) -> String {
        const VERSION: &'static str = env!("CARGO_PKG_VERSION");
        return VERSION.into();
    }

    pub fn insert(&mut self, col_name: &str, doc: Rc<Document>) -> DbResult<()> {
        self.ctx.insert(col_name, doc)
    }

    pub(crate) fn query_all_meta(&mut self) -> DbResult<Vec<Rc<Document>>> {
        self.ctx.query_all_meta()
    }

}

#[cfg(test)]
mod tests {
    use crate::Database;
    use std::rc::Rc;
    use crate::bson::{ Document, Value };

    #[test]
    fn test_create_collection() {
        let _ = std::fs::remove_file("/tmp/test.db");
        let _ = std::fs::remove_file("/tmp/test.db.journal");

        let mut db = Database::open("/tmp/test.db").unwrap();
        let result = db.create_collection("test").unwrap();
        println!("object:id {}", result.to_string());

        let meta = db.query_all_meta().unwrap();

        for (index, doc) in meta.iter().enumerate() {
            println!("index: {}, object: {}", index, doc)
        }

        for i in 0..100 {
            let content = i.to_string();
            let mut new_doc = Document::new_without_id();
            new_doc.insert("content".into(), Value::String(content));
            db.insert("test", Rc::new(new_doc)).unwrap();
        }

        let test_col_cursor = db.ctx.get_collection_cursor("test").unwrap();
        while test_col_cursor.has_next() {
            let doc = test_col_cursor.peek().unwrap();
            println!("object: {}", doc)
        }
    }

}