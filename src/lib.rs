mod error;
mod unprocessed_series;

pub use error::*;
pub extern crate gdal;

use gdal::vector::LayerAccess;
use gdal::Dataset;
use polars::prelude::*;
use std::collections::HashMap;
use std::ops::Deref;
use std::ops::DerefMut;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use unprocessed_series::*;

/// Parameters to configure the conversion of a vector dataset to a Polars DataFrame.
#[derive(Debug, Default)]
pub struct Params<'a> {
    /// GDal bitflags used by [`Dataset::open_ex`]. Flags are combined with a bitwise OR `|`.
    ///
    /// # Example
    ///
    /// ```rust
    /// use geopolars_gdal::gdal;
    ///
    /// let mut params = geopolars_gdal::Params::default();
    /// params.open_flags = gdal::GdalOpenFlags::GDAL_OF_READONLY | gdal::GdalOpenFlags::GDAL_OF_VERBOSE_ERROR;
    /// ```
    pub open_flags: gdal::GdalOpenFlags,

    /// List of allowed GDAL drivers. See https://gdal.org/drivers/vector/index.html
    pub allowed_drivers: Option<&'a [&'a str]>,

    /// Array of "KEY=value" strings to pass to the GDAL driver. See https://gdal.org/drivers/vector/index.html
    ///
    /// # Example
    /// ```rust
    /// use geopolars_gdal::gdal;
    ///
    /// let mut params = geopolars_gdal::Params::default();
    /// let csv_parsing_options = ["EMPTY_STRING_AS_NULL=YES", "KEEP_GEOM_COLUMNS=NO", "X_POSSIBLE_NAMES=Lon*", "Y_POSSIBLE_NAMES=Lat*"];
    /// params.open_options = Some(&csv_parsing_options);
    /// ```
    pub open_options: Option<&'a [&'a str]>,

    /// Array of strings that are filenames that are auxiliary to the main filename (eg .dbf .proj and .shx files are auxiliary to .shp files).
    ///
    /// If left as None, a probing of the file system will be done.
    pub sibling_files: Option<&'a [&'a str]>,

    /// For multi-layer files, the specific layer to read. If None, the first layer will be read.
    pub layer_name: Option<&'a str>,

    /// For muti-layer files, the specific muti-layer to read. If None, the first layer will be read.
    ///
    /// This has no effect is `layer_name` is set.
    pub layer_index: Option<usize>,

    /// The Feature ID column name. By default `fid` will be used. A empty string can be set to disable reading the feature id.
    pub fid_column_name: Option<&'a str>,

    /// The Geometry colum name. By default `geomery` will be used.
    ///
    /// Changing this is not recommended since the `geopolars` crates assumes geometries will be stored in the `geometry` column.
    pub geometry_column_name: Option<&'a str>,

    /// Stop reading after this many features. If None, all features will be read.
    pub truncating_limit: Option<usize>,

    /// The maximum number of features to read. If this limit is surpassed, an error will be returned.
    pub erroring_limit: Option<usize>,

    /// Start reading features at this offset.
    pub offset: Option<usize>,
}

impl<'a> Into<gdal::DatasetOptions<'a>> for &Params<'a> {
    fn into(self) -> gdal::DatasetOptions<'a> {
        gdal::DatasetOptions { 
            open_flags: self.open_flags, 
            allowed_drivers: self.allowed_drivers, 
            open_options: self.open_options, 
            sibling_files: self.sibling_files
        }
    }
}

struct LayerWrap<'a>(gdal::vector::Layer<'a>);

impl<'a> Deref for LayerWrap<'a> {
    type Target = gdal::vector::Layer<'a>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'a> DerefMut for LayerWrap<'a> {
    fn deref_mut(&mut self) -> &mut <Self as Deref>::Target {
        &mut self.0
    }
}

/// Given some raw bytes, create a dataframe.
///
/// Formats supported include GeoJSON, Shapefile, GPKG, and others.
/// See [https://gdal.org/drivers/vector/index.html](https://gdal.org/drivers/vector/index.html) for a full list of supported formats.
/// Some formats require additional libraries to be installed.
/// 
/// If using a dataformat that doesn't support named layers (eg GeoJSON) the default layer name will be 'layer'
///
/// # Example
/// ``` # ignore
/// use geopolars_gdal::bytes_to_df;
/// 
/// let geojson = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"foo"},"geometry":{"type":"Point","coordinates":[1,2]}},{"type":"Feature","properties":{"name":"bar"},"geometry":{"type":"Point","coordinates":[3,4]}}]}"#.as_bytes().to_vec();
/// let df = bytes_to_df(geojson, None).unwrap();
/// println!("{}", df);
/// ```
///
/// TODO: Support zipped, tared and gziped data.
pub fn bytes_to_df(bytes: Vec<u8>, params: Option<Params>) -> Result<DataFrame, Error> {
    static MEM_FILE_INCREMENTOR: AtomicU64 = AtomicU64::new(0);
    let params = params.unwrap_or_default();
    let gdal_options: gdal::DatasetOptions = (&params).into();

    let input_mem_path = format!(
        "/vsimem/geopolars_gdal/{}/{}/layer",
        std::process::id(),
        MEM_FILE_INCREMENTOR.fetch_add(1, Ordering::SeqCst)
    );
    gdal::vsi::create_mem_file(&input_mem_path, bytes)?;

    let dataset = gdal::Dataset::open_ex(&input_mem_path, gdal_options)?;
    let layer = if let Some(layer_name) = params.layer_name {
        dataset.layer_by_name(layer_name)?
    } else if let Some(layer_index) = params.layer_index {
        dataset.layer(layer_index as isize)?
    } else {
        dataset.layer(0)?
    };

    let mut wrap = LayerWrap(layer);

    layer_to_df(wrap.deref_mut(), Some(params))
}

/// Given a filepath, create a dataframe from that file.
///
/// Formats supported include GeoJSON, Shapefile, GPKG, and others.
/// See [https://gdal.org/drivers/vector/index.html](https://gdal.org/drivers/vector/index.html) for a full list of supported formats.
/// Some formats require additional libraries to be installed.
///
/// # Example
/// ``` # ignore
/// use geopolars_gdal::file_to_df;
/// let df = file_to_df("my_shapefile.shp", None).unwrap();
/// println!("{}", df);
/// ```
///
/// TODO: Support zipped, tared and gziped data.
pub fn file_to_df<P: AsRef<Path>>(
    path: P,
    params: Option<Params>,
) -> Result<DataFrame, Error> {
    let params = params.unwrap_or_default();
    let gdal_options: gdal::DatasetOptions = (&params).into();

    let dataset = Dataset::open_ex(path, gdal_options)?;

    let layer = if let Some(layer_name) = params.layer_name {
        dataset.layer_by_name(layer_name)?
    } else if let Some(layer_index) = params.layer_index {
        dataset.layer(layer_index as isize)?
    } else {
        dataset.layer(0)?
    };

    let mut wrap = LayerWrap(layer);

    layer_to_df(wrap.deref_mut(), Some(params))
}

/// Given a GDAL layer, create a dataframe.
///
/// This can be used to manually open a GDAL Dataset, and then create a dataframe from a specific layer.
/// This is most useful when you want to preprocess the Dataset in some way before creating a dataframe,
/// for example by applying a SQL filter or a spatial filter.
///
/// # Example
/// ``` # ignore
/// use geopolars_gdal::{layer_to_df, glal};
/// use gdal::vector::sql;
///
/// let dataset = gdal::Dataset::open("my_shapefile.shp")?;
/// let query = "SELECT kind, is_bridge, highway FROM my_shapefile WHERE highway = 'pedestrian'";
/// let mut result_set = dataset.execute_sql(query, None, sql::Dialect::DEFAULT).unwrap().unwrap();
///
/// let df = layer_to_df(result_set.deref_mut(), None);
/// println!("{}", df);
/// ```
pub fn layer_to_df<'l>(
    layer: &mut gdal::vector::Layer<'l>,
    params: Option<Params>,
) -> Result<DataFrame, Error> {
    let feat_count = layer.try_feature_count();

    let params = params.unwrap_or_default();
    let fid_column_name = params.fid_column_name.unwrap_or("fid");
    let geometry_column_name = params.geometry_column_name.unwrap_or("geometry");

    let mut unprocessed_series_map = HashMap::new();

    for (idx, feature) in &mut layer.features().enumerate() {
        if let Some(offset) = params.offset {
            if idx < offset {
                continue;
            }
        }
        if let Some(limit) = params.truncating_limit {
            if idx >= limit {
                break;
            }
        }
        if let Some(limit) = params.erroring_limit {
            if idx >= limit {
                return Err(Error::FeatureLimitReached(limit));
            }
        }

        if !fid_column_name.is_empty() {
            if let Some(fid) = feature.fid() {
                let entry = unprocessed_series_map
                    .entry(fid_column_name.to_owned())
                    .or_insert_with(|| UnprocessedSeries {
                        name: fid_column_name.to_owned(),
                        nullable: false,
                        datatype: UnprocessedDataType::Fid,
                        data: Vec::with_capacity(feat_count.unwrap_or(100) as usize),
                    });
                entry.data.push(GdalData::Fid(fid));
            }
        }

        for (idx, (name, value)) in feature.fields().enumerate() {
            if idx == 0 && name == geometry_column_name {
                return Err(Error::GeometryColumnCollision(
                    geometry_column_name.to_string(),
                ));
            }
            if idx == 0 && name == fid_column_name {
                return Err(Error::FidColumnCollision(fid_column_name.to_string()));
            }
            let entry = unprocessed_series_map
                .entry(name.clone())
                .or_insert_with(|| UnprocessedSeries {
                    name: name.clone(),
                    nullable: false,
                    datatype: gdal_type_to_unprocessed_type(&value),
                    data: Vec::with_capacity(feat_count.unwrap_or(100) as usize),
                });

            if value.is_none() && !entry.nullable {
                entry.nullable = true;
            }

            entry.data.push(GdalData::Value(value));
        }

        let entry = unprocessed_series_map
            .entry(geometry_column_name.to_owned())
            .or_insert_with(|| UnprocessedSeries {
                name: geometry_column_name.to_owned(),
                nullable: false,
                datatype: UnprocessedDataType::Geometry,
                data: Vec::with_capacity(feat_count.unwrap_or(100) as usize),
            });

        let wkb = feature.geometry().wkb()?;
        entry.data.push(GdalData::Geometry(wkb));
    }

    // Process the HashMap into a Vec of Series
    let mut series_vec = Vec::with_capacity(unprocessed_series_map.len());
    for (_, unprocessed_series) in unprocessed_series_map {
        if let UnprocessedDataType::Null = unprocessed_series.datatype {
            continue;
        }
        series_vec.push(unprocessed_series.process());
    }

    Ok(DataFrame::new(series_vec)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_to_df() {
        let _df = file_to_df(
            "test_data/us_states.feature_collection.implicit_4326.json",
            None,
        )
        .unwrap();
        //println!("{}", _df);

        let _df = file_to_df(
            "test_data/global_large_lakes.feature_collection.implicit_4326.json",
            None,
        )
        .unwrap();
        //println!("{}", _df);

        let _df = file_to_df("test_data/stations.shp", None).unwrap();
        // println!("{}", _df);
    }

    #[test]
    fn test_bytes_to_df() {
        let geojson = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"foo"},"geometry":{"type":"Point","coordinates":[1,2]}},{"type":"Feature","properties":{"name":"bar"},"geometry":{"type":"Point","coordinates":[3,4]}}]}"#.as_bytes().to_vec();
        let _df = bytes_to_df(geojson.clone(), None).unwrap();
        //println!("{}", df);
    }

    #[test]
    fn test_layer_to_df() {
        let geojson = r#"{"type":"FeatureCollection","features":[{"type":"Feature","properties":{"name":"foo"},"geometry":{"type":"Point","coordinates":[1,2]}},{"type":"Feature","properties":{"name":"bar"},"geometry":{"type":"Point","coordinates":[3,4]}}]}"#.as_bytes().to_vec();

        let input_mem_path = format!("/vsimem/geopolars_gdal/test_geojson/layer");
        gdal::vsi::create_mem_file(&input_mem_path, geojson).unwrap();
        let dataset = gdal::Dataset::open(&input_mem_path).unwrap();

        let query = "SELECT * FROM layer WHERE name = 'foo'";
        let mut result_set = dataset
            .execute_sql(query, None, gdal::vector::sql::Dialect::DEFAULT)
            .unwrap()
            .unwrap();

        let _df = layer_to_df(result_set.deref_mut(), None).unwrap();
        //println!("{}", _df);
    }
}