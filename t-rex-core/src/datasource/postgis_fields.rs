//
// Copyright (c) Pirmin Kalberer. All rights reserved.
// Licensed under the MIT License. See LICENSE file in the project root for full license information.
//

use crate::core::feature::{Feature, FeatureAttr, FeatureAttrValType};
use crate::core::geom::*;
use crate::core::layer::Layer;
use postgres::rows::Row;
use postgres::types::{self, FromSql, Type};
use std;

impl GeometryType {
    /// Convert returned geometry to core::geom::GeometryType based on GeometryType name
    pub fn from_geom_field(row: &Row, idx: &str, type_name: &str) -> Result<GeometryType, String> {
        let field = match type_name {
            //Option<Result<T>> --> Option<Result<GeometryType>>
            "POINT" => row
                .get_opt::<_, Point>(idx)
                .map(|opt| opt.map(|f| GeometryType::Point(f))),
            //"LINESTRING" =>
            //    row.get_opt::<_, LineString>(idx).map(|opt| opt.map(|f| GeometryType::LineString(f))),
            //"POLYGON" =>
            //    row.get_opt::<_, Polygon>(idx).map(|opt| opt.map(|f| GeometryType::Polygon(f))),
            "MULTIPOINT" => row
                .get_opt::<_, MultiPoint>(idx)
                .map(|opt| opt.map(|f| GeometryType::MultiPoint(f))),
            "LINESTRING" | "MULTILINESTRING" | "COMPOUNDCURVE" => row
                .get_opt::<_, MultiLineString>(idx)
                .map(|opt| opt.map(|f| GeometryType::MultiLineString(f))),
            "POLYGON" | "MULTIPOLYGON" | "CURVEPOLYGON" => row
                .get_opt::<_, MultiPolygon>(idx)
                .map(|opt| opt.map(|f| GeometryType::MultiPolygon(f))),
            "GEOMETRYCOLLECTION" => row
                .get_opt::<_, GeometryCollection>(idx)
                .map(|opt| opt.map(|f| GeometryType::GeometryCollection(f))),
            _ => {
                // PG geometry types:
                // CIRCULARSTRING, CIRCULARSTRINGM, COMPOUNDCURVE, COMPOUNDCURVEM, CURVEPOLYGON, CURVEPOLYGONM,
                // GEOMETRY, GEOMETRYCOLLECTION, GEOMETRYCOLLECTIONM, GEOMETRYM,
                // LINESTRING, LINESTRINGM, MULTICURVE, MULTICURVEM, MULTILINESTRING, MULTILINESTRINGM,
                // MULTIPOINT, MULTIPOINTM, MULTIPOLYGON, MULTIPOLYGONM, MULTISURFACE, MULTISURFACEM,
                // POINT, POINTM, POLYGON, POLYGONM,
                // POLYHEDRALSURFACE, POLYHEDRALSURFACEM, TIN, TINM, TRIANGLE, TRIANGLEM
                return Err(format!("Unknown geometry type {}", type_name));
            }
        };
        // Option<Result<GeometryType, _>> --> Result<GeometryType, String>
        field.map_or_else(
            || Err("Column not found".to_string()),
            |res| res.map_err(|err| format!("{}", err)),
        )
    }
}

impl FromSql for FeatureAttrValType {
    fn accepts(ty: &Type) -> bool {
        match ty {
            &types::VARCHAR
            | &types::TEXT
            | &types::CHAR_ARRAY
            | &types::FLOAT4
            | &types::FLOAT8
            | &types::INT2
            | &types::INT4
            | &types::INT8
            | &types::BOOL => true,
            _ => false,
        }
    }
    fn from_sql(ty: &Type, raw: &[u8]) -> Result<Self, Box<std::error::Error + Sync + Send>> {
        match ty {
            &types::VARCHAR | &types::TEXT | &types::CHAR_ARRAY => {
                <String>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::String(v)))
            }
            &types::FLOAT4 => {
                <f32>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Float(v)))
            }
            &types::FLOAT8 => {
                <f64>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Double(v)))
            }
            &types::INT2 => {
                <i16>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Int(v as i64)))
            }
            &types::INT4 => {
                <i32>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Int(v as i64)))
            }
            &types::INT8 => <i64>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Int(v))),
            &types::BOOL => <bool>::from_sql(ty, raw).and_then(|v| Ok(FeatureAttrValType::Bool(v))),
            _ => {
                let err: Box<std::error::Error + Sync + Send> =
                    format!("cannot convert {} to FeatureAttrValType", ty).into();
                Err(err)
            }
        }
    }
}

pub(crate) struct FeatureRow<'a> {
    pub layer: &'a Layer,
    pub row: &'a Row<'a>,
}

impl<'a> Feature for FeatureRow<'a> {
    fn fid(&self) -> Option<u64> {
        self.layer.fid_field.as_ref().and_then(|fid| {
            let val = self.row.get_opt::<_, FeatureAttrValType>(fid as &str);
            match val {
                Some(Ok(FeatureAttrValType::Int(fid))) => Some(fid as u64),
                _ => None,
            }
        })
    }
    fn attributes(&self) -> Vec<FeatureAttr> {
        let mut attrs = Vec::new();
        for (i, col) in self.row.columns().into_iter().enumerate() {
            // Skip geometry_field and fid_field
            if col.name()
                != self
                    .layer
                    .geometry_field
                    .as_ref()
                    .unwrap_or(&"".to_string())
                && col.name() != self.layer.fid_field.as_ref().unwrap_or(&"".to_string())
            {
                let val = self.row.get_opt::<_, Option<FeatureAttrValType>>(i);
                match val.unwrap() {
                    Ok(Some(v)) => {
                        let fattr = FeatureAttr {
                            key: col.name().to_string(),
                            value: v,
                        };
                        attrs.push(fattr);
                    }
                    Ok(None) => {
                        // Skip NULL values
                    }
                    Err(err) => {
                        warn!(
                            "Layer '{}' - skipping field '{}': {}",
                            self.layer.name,
                            col.name(),
                            err
                        );
                        //warn!("{:?}", self.row);
                    }
                }
            }
        }
        attrs
    }
    fn geometry(&self) -> Result<GeometryType, String> {
        let geom = GeometryType::from_geom_field(
            &self.row,
            &self
                .layer
                .geometry_field
                .as_ref()
                .expect("geometry_field undefined"),
            &self
                .layer
                .geometry_type
                .as_ref()
                .expect("geometry_type undefined"),
        );
        if let Err(ref err) = geom {
            error!("Layer '{}': {}", self.layer.name, err);
            error!("{:?}", self.row);
        }
        geom
    }
}
