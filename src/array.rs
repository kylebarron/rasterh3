use std::cmp::min;
use std::hash::Hash;

use ahash::HashMap;
use geo::MapCoords;
use geo_types::{Coord, Rect};
use h3o::geom::ToCells;
use h3o::{LatLng, Resolution};
use ndarray::{s, ArrayView2, Axis};
use rayon::prelude::*;
use tracing::debug;

use crate::resolution::{nearest_h3_resolution, ResolutionSearchMode};
use crate::{error::Error, transform::Transform, AxisOrder, CellSet};

fn find_continuous_chunks_along_axis<T>(
    a: &ArrayView2<T>,
    axis: usize,
    nodata_value: &T,
) -> Vec<(usize, usize)>
where
    T: Sized + PartialEq,
{
    let mut chunks = Vec::new();
    let mut current_chunk_start: Option<usize> = None;

    for (r0pos, r0) in a.axis_iter(Axis(axis)).enumerate() {
        if r0.iter().any(|v| v != nodata_value) {
            if current_chunk_start.is_none() {
                current_chunk_start = Some(r0pos);
            }
        } else if let Some(begin) = current_chunk_start {
            chunks.push((begin, r0pos - 1));
            current_chunk_start = None;
        }
    }
    if let Some(begin) = current_chunk_start {
        chunks.push((begin, a.shape()[axis] - 1));
    }
    chunks
}

/// Find all boxes in the array where there are any values except the `nodata_value`
///
/// This implementation is far from perfect and often recognizes multiple smaller
/// clusters as one as its based on completely empty columns and rows, but it is probably
/// sufficient for the purpose to reduce the number of hexagons
/// to be generated when dealing with fragmented/sparse datasets.
fn find_boxes_containing_data<T>(
    a: &ArrayView2<T>,
    nodata_value: &T,
    axis_order: &AxisOrder,
) -> Vec<Rect<usize>>
where
    T: Sized + PartialEq,
{
    find_continuous_chunks_along_axis(a, axis_order.x_axis(), nodata_value)
        .into_iter()
        .flat_map(|chunk_x_raw_indexes| {
            let sv = {
                let x_raw_range = chunk_x_raw_indexes.0..=chunk_x_raw_indexes.1;
                match axis_order {
                    AxisOrder::XY => a.slice(s![x_raw_range, ..]),
                    AxisOrder::YX => a.slice(s![.., x_raw_range]),
                }
            };
            find_continuous_chunks_along_axis(&sv, axis_order.y_axis(), nodata_value)
                .into_iter()
                .flat_map(move |chunks_y_raw_indexes| {
                    let sv2 = {
                        let x_raw_range = 0..=(chunk_x_raw_indexes.1 - chunk_x_raw_indexes.0);
                        let y_raw_range = chunks_y_raw_indexes.0..=chunks_y_raw_indexes.1;
                        match axis_order {
                            AxisOrder::XY => sv.slice(s![x_raw_range, y_raw_range]),
                            AxisOrder::YX => sv.slice(s![y_raw_range, x_raw_range]),
                        }
                    };

                    // one more iteration along axis 0 to get the specific range for that axis 1 range
                    find_continuous_chunks_along_axis(&sv2, axis_order.x_axis(), nodata_value)
                        .into_iter()
                        .map(move |chunks_x_indexes| {
                            Rect::new(
                                Coord {
                                    x: chunks_x_indexes.0 + chunk_x_raw_indexes.0,
                                    y: chunks_y_raw_indexes.0,
                                },
                                Coord {
                                    x: chunks_x_indexes.1 + chunk_x_raw_indexes.0,
                                    y: chunks_y_raw_indexes.1,
                                },
                            )
                        })
                })
        })
        .collect::<Vec<_>>()
}

/// Converts a two dimensional [`ndarray::ArrayView2`] to H3 cells.
///
/// The implementation tries to skip over regions with only nodata values.
pub struct H3Converter<'a, T>
where
    T: Sized + PartialEq + Sync + Eq + Hash,
{
    arr: &'a ArrayView2<'a, T>,
    nodata_value: &'a Option<T>,
    transform: &'a Transform,
    axis_order: AxisOrder,
}

impl<'a, T> H3Converter<'a, T>
where
    T: Sized + PartialEq + Sync + Eq + Hash,
{
    pub fn new(
        arr: &'a ArrayView2<'a, T>,
        nodata_value: &'a Option<T>,
        transform: &'a Transform,
        axis_order: AxisOrder,
    ) -> Self {
        Self {
            arr,
            nodata_value,
            transform,
            axis_order,
        }
    }

    /// Find the H3 resolution closest to the size of a pixel in an array,
    pub fn nearest_h3_resolution(
        &self,
        search_mode: ResolutionSearchMode,
    ) -> Result<Resolution, Error> {
        nearest_h3_resolution(
            self.arr.shape(),
            self.transform,
            &self.axis_order,
            search_mode,
        )
    }

    fn rects_with_data_with_nodata(&self, rect_size: usize, nodata: &T) -> Vec<Rect<usize>> {
        self.arr
            .axis_chunks_iter(Axis(self.axis_order.x_axis()), rect_size)
            .into_par_iter() // requires T to be Sync
            .enumerate()
            .map(|(axis_x_chunk_i, axis_x_chunk)| {
                let mut rects = Vec::new();
                for chunk_x_rect in
                    find_boxes_containing_data(&axis_x_chunk, nodata, &self.axis_order)
                {
                    let offset_x = (axis_x_chunk_i * rect_size) + chunk_x_rect.min().x;
                    let chunk_rect_view = {
                        let x_range = chunk_x_rect.min().x..chunk_x_rect.max().x;
                        let y_range = chunk_x_rect.min().y..chunk_x_rect.max().y;
                        match self.axis_order {
                            AxisOrder::XY => axis_x_chunk.slice(s![x_range, y_range]),
                            AxisOrder::YX => axis_x_chunk.slice(s![y_range, x_range]),
                        }
                    };
                    rects.extend(
                        chunk_rect_view
                            .axis_chunks_iter(Axis(self.axis_order.y_axis()), rect_size)
                            .enumerate()
                            .map(|(axis_y_chunk_i, axis_y_chunk)| {
                                let offset_y = (axis_y_chunk_i * rect_size) + chunk_x_rect.min().y;

                                // the window in array coordinates
                                Rect::new(
                                    Coord {
                                        x: offset_x,
                                        y: offset_y,
                                    },
                                    // add 1 to the max coordinate to include the whole last pixel
                                    Coord {
                                        x: (offset_x
                                            + axis_y_chunk.shape()[self.axis_order.x_axis()]
                                            + 1),
                                        y: (offset_y
                                            + axis_y_chunk.shape()[self.axis_order.y_axis()]
                                            + 1),
                                    },
                                )
                            }),
                    )
                }
                rects
            })
            .flatten()
            .collect()
    }

    fn rects_with_data_without_nodata(&self, rect_size: usize) -> Vec<Rect<usize>> {
        // just create tiles covering the complete array
        let x_size = self.arr.shape()[self.axis_order.x_axis()];
        let y_size = self.arr.shape()[self.axis_order.y_axis()];
        (0..((x_size as f64 / rect_size as f64).ceil() as usize))
            .flat_map(move |r_x| {
                (0..((y_size as f64 / rect_size as f64).ceil() as usize)).map(move |r_y| {
                    Rect::new(
                        Coord {
                            x: (r_x * rect_size),
                            y: (r_y * rect_size),
                        },
                        Coord {
                            x: (min(x_size, (r_x + 1) * rect_size)),
                            y: (min(y_size, (r_y + 1) * rect_size)),
                        },
                    )
                })
            })
            .collect()
    }

    fn rects_with_data(&self, rect_size: usize) -> Vec<Rect<usize>> {
        self.nodata_value.as_ref().map_or_else(
            || self.rects_with_data_without_nodata(rect_size),
            |nodata| self.rects_with_data_with_nodata(rect_size, nodata),
        )
    }

    pub fn to_h3(
        &self,
        h3_resolution: Resolution,
        compact: bool,
    ) -> Result<HashMap<&'a T, CellSet>, Error> {
        let inverse_transform = self.transform.invert()?;

        let rect_size = (self.arr.shape()[self.axis_order.x_axis()] / 10).clamp(10, 100);
        let rects = self.rects_with_data(rect_size);
        let n_rects = rects.len();
        debug!(
            "to_h3: found {} rects containing non-nodata values",
            n_rects
        );

        let chunk_h3_maps = rects
            .into_par_iter()
            .enumerate()
            .map(|(array_window_i, array_window)| {
                debug!(
                    "to_h3: rect {}/{} with size {} x {}",
                    array_window_i,
                    n_rects,
                    array_window.width(),
                    array_window.height()
                );

                let window = array_window.map_coords(|c| Coord::from((c.x as f64, c.y as f64)));
                // the window in geographical coordinates
                let window_box = self.transform * &window;

                convert_array_window(
                    self.arr,
                    window_box,
                    &inverse_transform,
                    self.axis_order,
                    self.nodata_value,
                    h3_resolution,
                    compact,
                )
            })
            .collect::<Result<Vec<_>, _>>()?;

        // combine the results from all chunks
        let mut h3_map = HashMap::default();
        for chunk_h3_map in chunk_h3_maps.into_iter() {
            for (value, mut cellset) in chunk_h3_map {
                h3_map
                    .entry(value)
                    .or_insert_with(CellSet::default)
                    .append(&mut cellset);
            }
        }

        finalize_chunk_map(&mut h3_map, compact)?;
        Ok(h3_map)
    }
}

fn convert_array_window<'a, T>(
    arr: &'a ArrayView2<'a, T>,
    window_box: Rect<f64>,
    inverse_transform: &Transform,
    axis_order: AxisOrder,
    nodata_value: &Option<T>,
    h3_resolution: Resolution,
    compact: bool,
) -> Result<HashMap<&'a T, CellSet>, Error>
where
    T: Sized + PartialEq + Sync + Eq + Hash,
{
    let mut chunk_h3_map = HashMap::<&T, CellSet>::default();
    let window_box = h3o::geom::Rect::from_degrees(window_box)?;
    for cell in window_box.to_cells(h3_resolution) {
        // find the array element for the coordinate of the h3 index
        let coord = {
            let ll: LatLng = cell.into();
            Coord::from((ll.lng_degrees(), ll.lat_degrees()))
        };
        let arr_coord = {
            let transformed = inverse_transform * coord;

            match axis_order {
                AxisOrder::XY => [
                    transformed.x.floor() as usize,
                    transformed.y.floor() as usize,
                ],
                AxisOrder::YX => [
                    transformed.y.floor() as usize,
                    transformed.x.floor() as usize,
                ],
            }
        };
        if let Some(value) = arr.get(arr_coord) {
            if let Some(nodata) = nodata_value {
                if nodata == value {
                    continue;
                }
            }
            chunk_h3_map
                .entry(value)
                .or_insert_with(CellSet::default)
                .insert(cell);
        }
    }

    // do an early compacting to free a bit of memory
    finalize_chunk_map(&mut chunk_h3_map, compact)?;

    Ok(chunk_h3_map)
}

fn finalize_chunk_map<T>(chunk_map: &mut HashMap<&T, CellSet>, compact: bool) -> Result<(), Error>
where
    T: Sync + Eq + Hash,
{
    chunk_map
        .par_iter_mut()
        .into_par_iter()
        .try_for_each(|(_, cellset)| cellset.finalize(compact))
}

#[cfg(test)]
mod tests {
    use ndarray::array;

    use crate::array::find_boxes_containing_data;
    use crate::{AxisOrder, H3Converter, ResolutionSearchMode, Transform};

    #[test]
    fn test_find_boxes_containing_data() {
        let arr = array![
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 1, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            [0, 1, 1, 0, 0, 0, 0, 1, 1, 1, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0],
            [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0],
            [0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 1, 1],
            [0, 0, 0, 1, 1, 0, 0, 0, 0, 0, 1, 1],
        ];
        let mut arr_copy = arr.clone();

        let n_elements = arr_copy.shape()[0] * arr_copy.shape()[1];
        let mut n_elements_in_boxes = 0;

        for rect in find_boxes_containing_data(&arr.view(), &0, &AxisOrder::YX) {
            n_elements_in_boxes +=
                (rect.max().x - rect.min().x + 1) * (rect.max().y - rect.min().y + 1);

            for x in rect.min().x..=rect.max().x {
                for y in rect.min().y..=rect.max().y {
                    arr_copy[(y, x)] = 0;
                }
            }
        }

        // there should be far less indexes to visit now
        assert!(n_elements_in_boxes < (n_elements / 2));

        // all elements should have been removed
        assert_eq!(arr_copy.sum(), 0);
    }

    #[test]
    fn preserve_nan_values() {
        use ordered_float::OrderedFloat;
        let arr = array![
            [OrderedFloat(f32::NAN), OrderedFloat(1.0_f32)],
            [OrderedFloat(f32::NAN), OrderedFloat(1.0_f32)],
        ];
        let transform = Transform::from_gdal(&[11.0, 1.0, 0.0, 10.0, 1.2, 0.2]);

        let view = arr.view();
        let converter = H3Converter::new(&view, &None, &transform, AxisOrder::XY);
        let h3_resolution = converter
            .nearest_h3_resolution(ResolutionSearchMode::SmallerThanPixel)
            .unwrap();
        let cell_map = converter.to_h3(h3_resolution, false).unwrap();
        assert!(cell_map.contains_key(&OrderedFloat(f32::NAN)));
        assert!(cell_map.contains_key(&OrderedFloat(1.0_f32)));
    }
}