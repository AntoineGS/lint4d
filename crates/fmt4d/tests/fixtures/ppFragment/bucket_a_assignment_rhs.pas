unit BucketAAssignmentRhs;

interface

implementation

procedure Schedule;
var
  n: Integer;
begin
  n := {$IFDEF DEBUG} 10 {$ELSE} 64 {$ENDIF};
end;

end.
