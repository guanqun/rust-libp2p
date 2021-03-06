// Copyright 2017 Parity Technologies (UK) Ltd.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

use Datastore;
use futures::Future;
use futures::stream::{Stream, iter_ok};
use parking_lot::Mutex;
use query::{Query, naive_apply_query};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, from_value, to_value, from_reader, to_writer};
use serde_json::value::Value;
use std::borrow::Cow;
use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::io::Error as IoError;
use std::io::ErrorKind as IoErrorKind;
use std::io::Read;
use std::path::PathBuf;
use tempfile::NamedTempFile;

/// Implementation of `Datastore` that uses a single plain JSON file.
pub struct JsonFileDatastore<T>
	where T: Serialize + DeserializeOwned
{
	path: PathBuf,
	content: Mutex<HashMap<String, T>>,
}

impl<T> JsonFileDatastore<T>
	where T: Serialize + DeserializeOwned
{
	/// Opens or creates the datastore. If the path refers to an existing path, then this function
	/// will attempt to load an existing set of values from it (which can result in an error).
	/// Otherwise if the path doesn't exist, a new empty datastore will be created.
	pub fn new<P>(path: P) -> Result<JsonFileDatastore<T>, IoError>
		where P: Into<PathBuf>
	{
		let path = path.into();

		if !path.exists() {
			return Ok(JsonFileDatastore {
				path: path,
				content: Mutex::new(HashMap::new()),
			});
		}

		let content = {
			let mut file = fs::File::open(&path)?;

			// We want to support empty files (and treat them as an empty recordset). Unfortunately
			// `serde_json` will always produce an error if we do this ("unexpected EOF at line 0
			// column 0"). Therefore we start by reading one byte from the file in order to check
			// for EOF.

			let mut first_byte = [0];
			if file.read(&mut first_byte)? == 0 {
				// File is empty.
				HashMap::new()
			} else {
				match from_reader::<_, Value>(Cursor::new(first_byte).chain(file)) {
					Ok(Value::Null) => HashMap::new(),
					Ok(Value::Object(map)) => {
						let mut out = HashMap::with_capacity(map.len());
						for (key, value) in map.into_iter() {
							let value = match from_value(value) {
								Ok(v) => v,
								Err(err) => return Err(IoError::new(IoErrorKind::InvalidData, err)),
							};
							out.insert(key, value);
						}
						out
					}
					Ok(_) => {
						return Err(IoError::new(IoErrorKind::InvalidData, "expected JSON object"));
					}
					Err(err) => {
						return Err(IoError::new(IoErrorKind::InvalidData, err));
					}
				}
			}
		};

		Ok(JsonFileDatastore {
			path: path,
			content: Mutex::new(content),
		})
	}

	/// Flushes the content of the datastore to the disk.
	///
	/// This function can only fail in case of a disk access error. If an error occurs, any change
	/// to the datastore that was performed since the last successful flush will be lost. No data
	/// will be corrupted.
	pub fn flush(&self) -> Result<(), IoError> {
		// Create a temporary file in the same directory as the destination, which avoids the
		// problem of having a file cleaner delete our file while we use it.
		let self_path_parent = self.path
								   .parent()
								   .ok_or(IoError::new(
			IoErrorKind::Other,
			"couldn't get parent directory of destination",
		))?;
		let mut temporary_file = NamedTempFile::new_in(self_path_parent)?;

		let content = self.content.lock();
		to_writer(
			&mut temporary_file,
			&content.iter().map(|(k, v)| (k.clone(), to_value(v).unwrap())).collect::<Map<_, _>>(),
		)?; // TODO: panic!
		temporary_file.sync_data()?;

		// Note that `persist` will fail if we try to persist across filesystems. However that
		// shouldn't happen since we created the temporary file in the same directory as the final
		// path.
		temporary_file.persist(&self.path)?;
		Ok(())
	}
}

impl<T> Datastore<T> for JsonFileDatastore<T>
	where T: Clone + Serialize + DeserializeOwned + Default + Ord + 'static
{
	#[inline]
	fn put(&self, key: Cow<str>, value: T) {
		let mut content = self.content.lock();
		content.insert(key.into_owned(), value);
	}

	fn get(&self, key: &str) -> Option<T> {
		let content = self.content.lock();
		// If the JSON is malformed, we just ignore the value.
		content.get(key).cloned()
	}

	fn has(&self, key: &str) -> bool {
		let content = self.content.lock();
		content.contains_key(key)
	}

	fn delete(&self, key: &str) -> bool {
		let mut content = self.content.lock();
		content.remove(key).is_some()
	}

	fn query<'a>(
		&'a self,
		query: Query<T>,
	) -> Box<Stream<Item = (String, T), Error = IoError> + 'a> {
		let content = self.content.lock();

		let keys_only = query.keys_only;

		let content_stream = iter_ok(content.iter().filter_map(|(key, value)| {
			// Skip values that are malformed.
			let value = if keys_only { Default::default() } else { value.clone() };

			Some((key.clone(), value))
		}));

		// `content_stream` reads from the content of the `Mutex`, so we need to clone the data
		// into a `Vec` before returning.
		let collected = naive_apply_query(content_stream, query)
			.collect()
			.wait()
			.expect("can only fail if either `naive_apply_query` or `content_stream` produce \
					 an error, which cann't happen");
		let output_stream = iter_ok(collected.into_iter());
		Box::new(output_stream) as Box<_>
	}
}

impl<T> Drop for JsonFileDatastore<T>
	where T: Serialize + DeserializeOwned
{
	#[inline]
	fn drop(&mut self) {
		// Unfortunately there's not much we can do here in case of an error, as panicking would be
		// very bad. Similar to `File`, the user should take care to call `flush()` before dropping
		// the datastore.
		//
		// If an error happens here, any change since the last successful flush will be lost, but
		// the data will not be corrupted.
		let _ = self.flush();
	}
}

#[cfg(test)]
mod tests {
	use {Query, Order, Filter, FilterTy, FilterOp};
	use Datastore;
	use JsonFileDatastore;
	use futures::{Future, Stream};
	use tempfile::NamedTempFile;

	#[test]
	fn open_and_flush() {
		let temp_file = NamedTempFile::new().unwrap();
		let datastore = JsonFileDatastore::<Vec<u8>>::new(temp_file.path()).unwrap();
		datastore.flush().unwrap();
	}

	#[test]
	fn values_store_and_reload() {
		let temp_file = NamedTempFile::new().unwrap();

		let datastore = JsonFileDatastore::<Vec<u8>>::new(temp_file.path()).unwrap();
		datastore.put("foo".into(), vec![1, 2, 3]);
		datastore.put("bar".into(), vec![0, 255, 127]);
		datastore.flush().unwrap();
		drop(datastore);

		let reload = JsonFileDatastore::<Vec<u8>>::new(temp_file.path()).unwrap();
		assert_eq!(reload.get("bar").unwrap(), &[0, 255, 127]);
		assert_eq!(reload.get("foo").unwrap(), &[1, 2, 3]);
	}

	#[test]
	fn query_basic() {
		let temp_file = NamedTempFile::new().unwrap();

		let datastore = JsonFileDatastore::<Vec<u8>>::new(temp_file.path()).unwrap();
		datastore.put("foo1".into(), vec![6, 7, 8]);
		datastore.put("foo2".into(), vec![6, 7, 8]);
		datastore.put("foo3".into(), vec![7, 8, 9]);
		datastore.put("foo4".into(), vec![10, 11, 12]);
		datastore.put("foo5".into(), vec![13, 14, 15]);
		datastore.put("bar1".into(), vec![0, 255, 127]);
		datastore.flush().unwrap();

		let query = datastore.query(Query {
			prefix: "fo".into(),
			filters: vec![
				Filter {
					ty: FilterTy::ValueCompare(&vec![6, 7, 8].into()),
					operation: FilterOp::NotEqual,
				},
			],
			orders: vec![Order::ByKeyDesc],
			skip: 1,
			limit: u64::max_value(),
			keys_only: false,
		})
							 .collect()
							 .wait()
							 .unwrap();

		assert_eq!(query[0].0, "foo4");
		assert_eq!(query[0].1, &[10, 11, 12]);
		assert_eq!(query[1].0, "foo3");
		assert_eq!(query[1].1, &[7, 8, 9]);
	}
}
