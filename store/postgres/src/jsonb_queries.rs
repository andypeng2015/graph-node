///! This module generates queries for the JSONB storage scheme.
///
///  We really only need that for supporting `EntityQuery`
use diesel::pg::Pg;
use diesel::prelude::BoxableExpression;
use diesel::query_builder::{AstPass, Query, QueryFragment, QueryId};
use diesel::query_dsl::RunQueryDsl;
use diesel::result::QueryResult;
use diesel::sql_types::{Array, Bool, Jsonb, Nullable, Text};
use graph::prelude::{
    EntityCollection, EntityFilter, EntityLink, EntityOrder, EntityRange, EntityWindow, ParentLink,
    QueryExecutionError, ValueType, WindowAttribute,
};

use crate::entities::{EntityTable, STRING_PREFIX_SIZE};
use crate::filter::build_filter;
use crate::relational::PRIMARY_KEY_COLUMN;

pub struct OrderDetails {
    attribute: String,
    cast: &'static str,
    prefix_only: bool,
    direction: EntityOrder,
}

pub struct FilterQuery<'a> {
    table: &'a EntityTable,
    collection: EntityCollection,
    filter: Option<Box<dyn BoxableExpression<EntityTable, Pg, SqlType = Bool>>>,
    order: Option<OrderDetails>,
    range: EntityRange,
}

impl<'a> FilterQuery<'a> {
    pub fn new(
        table: &'a EntityTable,
        collection: EntityCollection,
        filter: Option<EntityFilter>,
        order: Option<(String, ValueType, EntityOrder)>,
        range: EntityRange,
    ) -> Result<Self, QueryExecutionError> {
        let order = if let Some((attribute, value_type, direction)) = order {
            let cast = match value_type {
                ValueType::BigInt | ValueType::BigDecimal => "::numeric",
                ValueType::Boolean => "::boolean",
                ValueType::Bytes => "",
                ValueType::ID => "",
                ValueType::Int => "::bigint",
                ValueType::String => "",
                ValueType::List => {
                    return Err(QueryExecutionError::OrderByNotSupportedForType(
                        "List".to_string(),
                    ));
                }
            };

            let prefix_only = &attribute != PRIMARY_KEY_COLUMN && value_type == ValueType::String;
            Some(OrderDetails {
                attribute,
                cast,
                prefix_only,
                direction,
            })
        } else {
            None
        };
        let filter = if let Some(filter) = filter {
            Some(build_filter(filter).map_err(|e| {
                QueryExecutionError::FilterNotSupportedError(format!("{}", e.value), e.filter)
            })?)
        } else {
            None
        };
        Ok(FilterQuery {
            table,
            collection,
            filter,
            order,
            range,
        })
    }

    fn entities_clause(&self, entities: &Vec<String>, out: &mut AstPass<Pg>) -> QueryResult<()> {
        if entities.len() == 1 {
            // If there is only one entity_type, which is the case in all
            // queries that do not involve interfaces, leaving out `any`
            // lets Postgres use the primary key index on the entities table
            let entity_type = entities
                .first()
                .expect("we checked that there is exactly one entity_type");
            out.push_sql("entity = ");
            out.push_bind_param::<Text, _>(&entity_type)?;
        } else {
            out.push_sql("entity = any(");
            out.push_bind_param::<Array<Text>, _>(&entities)?;
            out.push_sql(")");
        }
        Ok(())
    }

    fn order_by(&self, out: &mut AstPass<Pg>) -> QueryResult<()> {
        out.push_sql("\n order by ");
        if let Some(order) = &self.order {
            if order.prefix_only {
                out.push_sql("left(data ->");
                out.push_bind_param::<Text, _>(&order.attribute)?;
                out.push_sql("->> 'data', ");
                out.push_sql(&STRING_PREFIX_SIZE.to_string());
                out.push_sql(") ");
            } else {
                if &order.attribute == PRIMARY_KEY_COLUMN {
                    out.push_identifier(PRIMARY_KEY_COLUMN)?;
                } else {
                    out.push_sql("(data ->");
                    out.push_bind_param::<Text, _>(&order.attribute)?;
                    out.push_sql("->> 'data')");
                    out.push_sql(&order.cast);
                }
                out.push_sql(" ");
            }
            out.push_sql(order.direction.to_sql());
            out.push_sql(" nulls last, ");
        }
        out.push_identifier(PRIMARY_KEY_COLUMN)
    }

    fn limit(&self, out: &mut AstPass<Pg>) {
        if let Some(first) = &self.range.first {
            out.push_sql("\n limit ");
            out.push_sql(&first.to_string());
        }
        if self.range.skip > 0 {
            out.push_sql("\noffset ");
            out.push_sql(&self.range.skip.to_string());
        }
    }

    /// Generate the query when there is no window. This produces
    ///
    ///   select data, entity
    ///     from {table}
    ///    where entity = any({entity_types})
    ///      and {filter}
    ///    order by {order}
    ///    limit {range.first} offset {range.skip}
    fn query_no_window(&self, entities: &Vec<String>, mut out: AstPass<Pg>) -> QueryResult<()> {
        out.unsafe_to_cache_prepared();
        out.push_sql("select id, data, entity");
        out.push_sql("\n  from ");
        self.table.walk_ast(out.reborrow())?;
        out.push_sql(" c\n where ");
        self.entities_clause(entities, &mut out)?;
        if let Some(filter) = &self.filter {
            out.push_sql(" and ");
            filter.walk_ast(out.reborrow())?;
        }

        self.order_by(&mut out)?;
        self.limit(&mut out);
        Ok(())
    }

    fn expand_parents(&self, window: &EntityWindow, out: &mut AstPass<Pg>) -> QueryResult<()> {
        match &window.link {
            EntityLink::Direct(_) => {
                // Type A and B
                // (select * from unnest($parent_ids)) as p(id)
                out.push_sql("(select * from unnest(");
                out.push_bind_param::<Array<Text>, _>(&window.ids)?;
                out.push_sql(")) as p(id)");
            }
            EntityLink::Parent(ParentLink::List(child_ids)) => {
                // Type C
                // child_ids is a Vec<Vec<String>>; Postgres will only
                // accept that if all Vec<String> are the same length. We
                // therefore pad shorter ones with None, which become
                // nulls in the database
                let maxlen = child_ids.iter().map(|ids| ids.len()).max().unwrap_or(0);
                let child_ids = child_ids
                    .into_iter()
                    .map(|ids| {
                        let mut ids: Vec<_> = ids.into_iter().map(Some).collect();
                        ids.resize_with(maxlen, || None);
                        ids
                    })
                    .collect::<Vec<_>>();

                // (select * from unnest($parent_ids, $child_id_matrix)) as p(id, child_ids)
                out.push_sql("(select * from unnest(");
                out.push_bind_param::<Array<Text>, _>(&window.ids)?;
                out.push_sql(", ");
                out.push_bind_param::<Array<Array<Nullable<Text>>>, _>(&child_ids)?;
                out.push_sql(") as p(id, child_ids)");
            }
            EntityLink::Parent(ParentLink::Scalar(child_ids)) => {
                // Type D
                // (select * from unnest($parent_ids, $child_ids)) as p(id, child_id)
                out.push_sql("(select * from unnest(");
                out.push_bind_param::<Array<Text>, _>(&window.ids)?;
                out.push_sql(",");
                out.push_bind_param::<Array<Text>, _>(&child_ids)?;
                out.push_sql(")) as p(id, child_id)");
            }
        }
        Ok(())
    }

    fn linked_children(&self, window: &EntityWindow, out: &mut AstPass<Pg>) -> QueryResult<()> {
        match &window.link {
            EntityLink::Direct(WindowAttribute::List(name)) => {
                // Type A
                // The `in (..)` part turns the id's stored in `name` into
                // a list of parent ids
                out.push_sql("p.id in (select ary->>'data' from jsonb_array_elements(c.data->");
                out.push_bind_param::<Text, _>(name)?;
                out.push_sql("->'data') ary)");
            }
            EntityLink::Direct(WindowAttribute::Scalar(name)) => {
                // Type B
                // p.id = c.data->{name}->>'data'
                out.push_sql("p.id = c.data->");
                out.push_bind_param::<Text, _>(name)?;
                out.push_sql("->>'data'");
            }
            EntityLink::Parent(ParentLink::List(_)) => {
                // Type C
                out.push_sql("c.id = any(p.child_ids)");
            }
            EntityLink::Parent(ParentLink::Scalar(_)) => {
                // Type D
                out.push_sql("c.id = p.child_id");
            }
        }
        Ok(())
    }

    /// Generate the query when there is a window. Since we might have
    /// different filters for each entity type, we need to write this as
    /// a `union all` and can't just use `any({entity_types})` as in the
    /// `query_no_window`
    ///
    /// The query we produce is
    ///
    ///   select id, data, entity, parent_id
    ///     from {expand_parents}
    ///          cross join lateral
    ///            (select id, data, entity
    ///               from entities c
    ///              where {linked_children}
    ///                and c.entity = {child_type}
    ///                and {filter})
    ///   union all
    ///   .. range over all windows ..
    ///   order by {order}
    ///   limit {first} skip {skip}    
    ///         
    fn query_window(&self, windows: &Vec<EntityWindow>, mut out: AstPass<Pg>) -> QueryResult<()> {
        out.unsafe_to_cache_prepared();

        for (index, window) in windows.iter().enumerate() {
            if index > 0 {
                out.push_sql("\nunion all\n");
            }
            // we actually put the parent_id into the entity as g$parent_id
            out.push_sql(
                "select c.id, \
                c.data || jsonb_build_object('g$parent_id', \
                          jsonb_build_object('data', p.id, \
                                             'type', 'String'))
                c.entity ",
            );
            self.expand_parents(window, &mut out)?;
            out.push_sql(
                " cross join lateral \
                 (select c.id, c.data, c.entity \
                 from ",
            );
            self.table.walk_ast(out.reborrow())?;
            out.push_sql(" c");
            out.push_sql(" where ");
            self.linked_children(window, &mut out)?;
            out.push_sql(" and c.entity = ");
            out.push_bind_param::<Text, _>(&window.child_type)?;
            if let Some(filter) = &self.filter {
                out.push_sql(" and ");
                filter.walk_ast(out.reborrow())?;
            }
        }
        self.order_by(&mut out)?;
        self.limit(&mut out);
        Ok(())
    }
}

impl<'a> QueryFragment<Pg> for FilterQuery<'a> {
    fn walk_ast(&self, out: AstPass<Pg>) -> QueryResult<()> {
        match &self.collection {
            EntityCollection::All(entities) => self.query_no_window(entities, out),
            EntityCollection::Window(windows) => self.query_window(windows, out),
        }
    }
}

impl<'a> QueryId for FilterQuery<'a> {
    type QueryId = ();

    const HAS_STATIC_QUERY_ID: bool = false;
}

impl<'a> Query for FilterQuery<'a> {
    type SqlType = (Text, Jsonb, Text);
}

impl<'a, Conn> RunQueryDsl<Conn> for FilterQuery<'a> {}
